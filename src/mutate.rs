// Mutation compilers (insert/update/delete/put/upsert) and RPC calls
// (including procedures, overloads, bulk bodies and result shaping).
use crate::ast::{Cond, Query, Sel};
use crate::compiler::{compile_conds, compile_return_read, RpcShape};
use crate::error::ApiError;
use crate::schema::{FnInfo, RetKind, SchemaCache, TableInfo};
use crate::sql::{json_literal, qi, ql};
use std::collections::HashSet;

pub struct MutationSql {
    pub sql: String,      // complete statement (CTE-wrapped when projecting)
    pub wants_rows: bool, // return=representation
}

#[derive(Clone, Copy, PartialEq)]
pub enum Resolution {
    Merge,
    Ignore,
    None,
}

// ---------- shared helpers ----------

/// Render the INSERT/UPDATE/DELETE statement plus optional CTE projection.
fn finish_mutation(
    base: String,
    select: &[Sel],
    cache: &SchemaCache,
    t: &TableInfo,
    wants_rows: bool,
) -> Result<MutationSql, ApiError> {
    if select.iter().any(|s| s.agg.is_some()) {
        return Err(ApiError::parse(
            "aggregate functions cannot be used in the select of a mutation",
        ));
    }
    if !wants_rows {
        return Ok(MutationSql { sql: base, wants_rows: false });
    }
    if select.is_empty() {
        return Ok(MutationSql {
            sql: format!("{} RETURNING to_json(\"t\") AS \"row\"", base),
            wants_rows: true,
        });
    }
    let outer = compile_return_read(cache, t, select)?;
    Ok(MutationSql {
        sql: format!("WITH \"pgrest_src\" AS ({} RETURNING *) {}", base, outer),
        wants_rows: true,
    })
}

fn validate_mutation_cols(t: &TableInfo, cols: impl Iterator<Item = String>) -> Result<(), ApiError> {
    for c in cols {
        match t.col(&c) {
            None => {
                return Err(ApiError::new(
                    400,
                    "PG42703",
                    format!("column '{}' of relation '{}' does not exist", c, t.name),
                ))
            }
            Some(info) if info.generated => {
                return Err(ApiError::new(
                    400,
                    "PG42601",
                    format!("cannot modify generated column '{}'", c),
                ))
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn compile_insert(
    cache: &SchemaCache,
    t: &TableInfo,
    rows: &[serde_json::Map<String, serde_json::Value>],
    columns_opt: Option<&[String]>,
    on_conflict: Option<&[String]>,
    resolution: Resolution,
    missing_default: bool,
    wants_rows: bool,
    select: &[Sel],
) -> Result<MutationSql, ApiError> {
    if rows.is_empty() {
        return Err(ApiError::body_required());
    }
    let columns: Vec<String> = match columns_opt {
        Some(c) => c
            .iter()
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => rows[0].keys().cloned().collect(),
    };
    if columns.is_empty() {
        return Err(ApiError::body_required());
    }
    validate_mutation_cols(t, columns.iter().cloned())?;
    let mut values: Vec<String> = Vec::new();
    for row in rows {
        let vals: Vec<String> = columns
            .iter()
            .map(|c| match row.get(c) {
                Some(v) => json_literal(v),
                None => {
                    if missing_default {
                        "DEFAULT".to_string()
                    } else {
                        "NULL".to_string()
                    }
                }
            })
            .collect();
        values.push(format!("({})", vals.join(", ")));
    }

    let mut base = format!(
        "INSERT INTO {}.{} AS \"t\" ({}) VALUES {}",
        qi(&t.schema),
        qi(&t.name),
        columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", "),
        values.join(", ")
    );

    if resolution != Resolution::None {
        let conflict_cols: Vec<String> = match on_conflict {
            Some(c) if !c.is_empty() => c.to_vec(),
            _ => {
                if t.pks.is_empty() {
                    return Err(ApiError::new(
                        400,
                        "PGRST102",
                        "upsert requires '?on_conflict=' or a primary key on the table",
                    ));
                }
                t.pks.clone()
            }
        };
        match resolution {
            Resolution::Ignore => {
                base.push_str(&format!(
                    " ON CONFLICT ({}) DO NOTHING",
                    conflict_cols.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ")
                ));
            }
            Resolution::Merge => {
                let update_cols: Vec<&String> = columns
                    .iter()
                    .filter(|c| !conflict_cols.contains(c) && !t.pks.contains(c))
                    .collect();
                if update_cols.is_empty() {
                    base.push_str(&format!(
                        " ON CONFLICT ({}) DO NOTHING",
                        conflict_cols.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ")
                    ));
                } else {
                    base.push_str(&format!(
                        " ON CONFLICT ({}) DO UPDATE SET {}",
                        conflict_cols.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", "),
                        update_cols
                            .iter()
                            .map(|c| format!("{} = EXCLUDED.{}", qi(c), qi(c)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Resolution::None => {}
        }
    }

    // always RETURN so Location can be computed; body shaped per preference
    let m = finish_mutation(base, select, cache, t, true)?;
    Ok(MutationSql { sql: m.sql, wants_rows })
}
/// PUT: update-or-insert by primary key. Requires the PK columns both in the
/// filter (eq conditions) and in the body, PostgREST-style.
pub fn compile_put(
    cache: &SchemaCache,
    t: &TableInfo,
    q: &Query,
    body: &serde_json::Map<String, serde_json::Value>,
    wants_rows: bool,
    select: &[Sel],
) -> Result<MutationSql, ApiError> {
    if t.pks.is_empty() {
        return Err(ApiError::new(
            405,
            "PGRST102",
            "PUT is only supported on tables with a primary key",
        ));
    }
    if body.is_empty() {
        return Err(ApiError::body_required());
    }
    for pk in &t.pks {
        if !body.contains_key(pk) {
            return Err(ApiError::new(
                400,
                "PGRST102",
                format!("PUT requires the primary key column '{}' in the body", pk),
            ));
        }
        if !filter_covers_eq(&q.conds, pk) {
            return Err(ApiError::new(
                400,
                "PGRST102",
                format!("PUT must be filtered by the primary key column '{}' (eq)", pk),
            ));
        }
    }
    let columns: Vec<String> = body.keys().cloned().collect();
    validate_mutation_cols(t, columns.iter().cloned())?;

    let sets: Vec<String> = columns
        .iter()
        .map(|c| format!("{} = {}", qi(c), json_literal(&body[c])))
        .collect();
    let wh = compile_conds(&q.conds, "t")?;
    if wh.is_empty() {
        return Err(ApiError::filters_required());
    }

    let update = format!(
        "UPDATE {}.{} AS \"t\" SET {} WHERE {}",
        qi(&t.schema),
        qi(&t.name),
        sets.join(", "),
        wh
    );
    let cols_list = columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");
    let vals_sql = columns.iter().map(|c| json_literal(&body[c])).collect::<Vec<_>>().join(", ");
    // INSERT ... SELECT form so the guard can reference the UPDATE CTE
    let insert = format!(
        "INSERT INTO {sch}.{tbl} ({cols}) SELECT * FROM (VALUES ({vals})) AS \"v\"({cols2}) WHERE NOT EXISTS (SELECT 1 FROM \"pgrest_src\")",
        sch = qi(&t.schema),
        tbl = qi(&t.name),
        cols = cols_list,
        vals = vals_sql,
        cols2 = cols_list
    );

    if !wants_rows {
        let sql = format!(
            "WITH \"pgrest_src\" AS ({upd} RETURNING 1), \"pgrest_ins\" AS ({ins} RETURNING 1) SELECT 1",
            upd = update,
            ins = insert,
        );
        return Ok(MutationSql { sql, wants_rows: false });
    }

    let outer = compile_return_read(cache, t, select)?;
    let outer_ins = outer.replace("\"pgrest_src\"", "\"pgrest_ins\"");
    let sql = format!(
        "WITH \"pgrest_src\" AS ({upd} RETURNING *), \"pgrest_ins\" AS ({ins} RETURNING *) {oins} UNION ALL {oupd}",
        upd = update,
        ins = insert,
        oins = outer_ins,
        oupd = outer,
    );
    Ok(MutationSql { sql, wants_rows: true })
}

/// True when `conds` contains a non-negated `col = eq.<value>` leaf.
fn filter_covers_eq(conds: &[Cond], col: &str) -> bool {
    conds.iter().any(|c| match c {
        Cond::Leaf { col: c, op, not, .. } => c == col && op == "eq" && !not,
        Cond::Group { items, .. } => filter_covers_eq(items, col),
    })
}

pub fn compile_update(
    cache: &SchemaCache,
    t: &TableInfo,
    q: &Query,
    body: &serde_json::Map<String, serde_json::Value>,
    wants_rows: bool,
    select: &[Sel],
) -> Result<MutationSql, ApiError> {
    if body.is_empty() {
        return Err(ApiError::body_required());
    }
    validate_mutation_cols(t, body.keys().cloned())?;
    let sets: Vec<String> = body
        .iter()
        .map(|(k, v)| format!("{} = {}", qi(k), json_literal(v)))
        .collect();
    let wh = compile_conds(&q.conds, "t")?;
    if wh.is_empty() {
        return Err(ApiError::filters_required());
    }
    let base = format!(
        "UPDATE {}.{} AS \"t\" SET {} WHERE {}",
        qi(&t.schema),
        qi(&t.name),
        sets.join(", "),
        wh
    );
    finish_mutation(base, select, cache, t, wants_rows)
}

pub fn compile_delete(
    cache: &SchemaCache,
    t: &TableInfo,
    q: &Query,
    wants_rows: bool,
    select: &[Sel],
) -> Result<MutationSql, ApiError> {
    let wh = compile_conds(&q.conds, "t")?;
    if wh.is_empty() {
        return Err(ApiError::filters_required());
    }
    let base = format!(
        "DELETE FROM {}.{} AS \"t\" WHERE {}",
        qi(&t.schema),
        qi(&t.name),
        wh
    );
    finish_mutation(base, select, cache, t, wants_rows)
}

// ---------- RPC ----------

pub struct RpcSql {
    pub sql: String,
    pub wants_rows: bool,
    pub scalar_text: bool,
}

fn render_arg(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => ql(s),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => v.to_string(),
        serde_json::Value::Null => "NULL".to_string(),
        other => ql(&other.to_string()),
    }
}

fn render_call(f: &FnInfo, args: &serde_json::Map<String, serde_json::Value>) -> String {
    let parts: Vec<String> = f
        .arg_names
        .iter()
        .filter_map(|name| args.get(name).map(|v| format!("{} => {}", qi(name), render_arg(v))))
        .collect();
    format!("{}.{}({})", qi(&f.schema), qi(&f.name), parts.join(", "))
}

/// Choose the right overload given the argument names the caller provided.
/// Candidates must have every provided arg and be able to run with the given
/// arity (provided count between `required` and `arg_names.len()`); an exact
/// arity match wins. Errors with PostgREST's ambiguity codes otherwise.
pub fn pick_overload<'a>(
    candidates: &[&'a FnInfo],
    provided: &HashSet<String>,
) -> Result<&'a FnInfo, ApiError> {
    let n = provided.len();
    let matching: Vec<&&FnInfo> = candidates
        .iter()
        .filter(|f| provided.iter().all(|k| f.arg_names.contains(k)))
        .filter(|f| {
            // arity plausibility: callers may omit args that have defaults,
            // but cannot omit required ones
            f.required <= n && n <= f.arg_names.len()
        })
        .collect();
    match matching.len() {
        1 => Ok(matching[0]),
        0 => {
            if candidates.len() == 1 {
                Ok(candidates[0])
            } else {
                Err(ApiError::new(
                    404,
                    "PGRST202",
                    "Could not find the function with a signature matching the provided arguments in the schema cache",
                ))
            }
        }
        _ => {
            if let Some(exact) = matching.iter().find(|f| f.arg_names.len() == n && f.required == n) {
                Ok(exact)
            } else if let Some(exact) = matching.iter().find(|f| f.arg_names.len() == n) {
                Ok(exact)
            } else {
                let names = matching
                    .iter()
                    .map(|f| format!("{}({})", f.name, f.arg_names.join(", ")))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(ApiError::new(
                    300,
                    "PGRST203",
                    format!("Could not choose the best candidate function between: {}", names),
                )
                .with_hint("Try renaming the function or providing the full set of arguments"))
            }
        }
    }
}

/// One RPC call (or N calls for `Prefer: params=multiple-objects`).
/// `shape` carries select/order/limit/offset/filters used to shape setof results.
pub fn compile_rpc(
    cache: &SchemaCache,
    candidates: &[&FnInfo],
    args_list: &[serde_json::Map<String, serde_json::Value>],
    shape: Option<&RpcShape>,
) -> Result<RpcSql, ApiError> {
    if args_list.is_empty() {
        return Err(ApiError::body_required());
    }
    let provided: HashSet<String> = args_list[0].keys().cloned().collect();
    let f = pick_overload(candidates, &provided)?;

    // procedures are invoked with CALL and never return a body
    if f.is_procedure {
        let call = render_call(f, &args_list[0]);
        return Ok(RpcSql { sql: format!("CALL {}", call), wants_rows: false, scalar_text: false });
    }

    let single = |args: &serde_json::Map<String, serde_json::Value>| render_call(f, args);

    let sql = match &f.ret_kind {
        RetKind::Void => format!("SELECT {}", single(&args_list[0])),
        RetKind::Scalar(tyname) => {
            let is_text = tyname == "text"
                || tyname == "character varying"
                || tyname.starts_with("character");
            if args_list.len() == 1 {
                let call = single(&args_list[0]);
                if is_text {
                    format!("SELECT {}::text AS \"row\"", call)
                } else {
                    format!("SELECT to_json({}) AS \"row\"", call)
                }
            } else {
                // bulk scalar calls: one row per object
                let unions: Vec<String> = args_list
                    .iter()
                    .map(|args| {
                        let call = render_call(f, args);
                        if is_text {
                            format!("SELECT {}::text AS \"row\"", call)
                        } else {
                            format!("SELECT to_json({}) AS \"row\"", call)
                        }
                    })
                    .collect();
                unions.join(" UNION ALL ")
            }
        }
        RetKind::Composite(_, _) => {
            // scalar composite: a single JSON object
            format!("SELECT to_json({}) AS \"row\"", single(&args_list[0]))
        }
        RetKind::SetOfScalar(_) => {
            // setof scalar: one JSON scalar per row (array-shaped by the handler)
            if args_list.len() == 1 {
                format!("SELECT to_json({}) AS \"row\"", single(&args_list[0]))
            } else {
                let unions: Vec<String> = args_list
                    .iter()
                    .map(|args| format!("SELECT to_json({}) AS \"row\"", single(args)))
                    .collect();
                unions.join(" UNION ALL ")
            }
        }
        RetKind::SetOf(_, _) | RetKind::Table => {
            // setof composite / TABLE: one JSON object per row, optionally shaped
            let shaped = args_list.len() == 1 && shape.is_some();
            if shaped {
                crate::compiler::compile_rpc_shaping(cache, &single(&args_list[0]), shape.unwrap())?
            } else if args_list.len() == 1 {
                format!("SELECT to_json(\"r\") AS \"row\" FROM {} \"r\"", single(&args_list[0]))
            } else {
                let unions: Vec<String> = args_list
                    .iter()
                    .map(|args| format!("SELECT to_json(\"r\") AS \"row\" FROM {} \"r\"", single(args)))
                    .collect();
                unions.join(" UNION ALL ")
            }
        }
    };

    Ok(RpcSql {
        sql,
        wants_rows: f.ret_kind != RetKind::Void,
        scalar_text: matches!(
            &f.ret_kind,
            RetKind::Scalar(t) if t == "text" || t.starts_with("character")
        ),
    })
}

// for text_literal re-export
#[allow(unused_imports)]
use crate::sql::text_literal as _tl;
