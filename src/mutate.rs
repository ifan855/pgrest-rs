// Mutation compilers (insert/update/delete/upsert) and RPC calls.
use crate::ast::Query;
use crate::compiler::compile_conds;
use crate::error::ApiError;
use crate::schema::{FnInfo, RetKind, SchemaCache, TableInfo};
use crate::sql::{json_literal, qi, ql};
pub struct MutationSql {
    pub sql: String,          // main statement (with RETURNING json)
    pub wants_rows: bool,     // return=representation
}

#[derive(Clone, Copy, PartialEq)]
pub enum Resolution {
    Merge,
    Ignore,
    None,
}

pub fn compile_insert(
    t: &TableInfo,
    rows: &[serde_json::Map<String, serde_json::Value>],
    columns_opt: Option<&[String]>,
    on_conflict: Option<&[String]>,
    resolution: Resolution,
    missing_default: bool,
    wants_rows: bool,
) -> Result<MutationSql, ApiError> {
    if rows.is_empty() {
        return Err(ApiError::body_required());
    }
    let columns: Vec<String> = match columns_opt {
        Some(c) => c.to_vec(),
        None => rows[0].keys().cloned().collect(),
    };
    if columns.is_empty() {
        return Err(ApiError::body_required());
    }
    // validate columns exist
    for c in &columns {
        if t.col(c).is_none() {
            return Err(ApiError::new(
                400,
                "PG42703",
                format!("column '{}' of relation '{}' does not exist", c, t.name),
            ));
        }
    }
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

    let mut sql = format!(
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
                sql.push_str(&format!(
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
                    sql.push_str(&format!(
                        " ON CONFLICT ({}) DO NOTHING",
                        conflict_cols.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ")
                    ));
                } else {
                    sql.push_str(&format!(
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

    // always RETURN so Location can be computed; body shaped per return preference
    sql.push_str(" RETURNING to_json(\"t\") AS \"row\"");
    Ok(MutationSql { sql, wants_rows })
}

pub fn compile_update(
    t: &TableInfo,
    q: &Query,
    body: &serde_json::Map<String, serde_json::Value>,
    wants_rows: bool,
) -> Result<MutationSql, ApiError> {
    if body.is_empty() {
        return Err(ApiError::body_required());
    }
    let mut sets: Vec<String> = Vec::new();
    for (k, v) in body {
        if t.col(k).is_none() {
            return Err(ApiError::new(
                400,
                "PG42703",
                format!("column '{}' of relation '{}' does not exist", k, t.name),
            ));
        }
        if t.col(k).map(|c| c.generated).unwrap_or(false) {
            return Err(ApiError::new(
                400,
                "PG42601",
                format!("cannot UPDATE generated column '{}'", k),
            ));
        }
        sets.push(format!("{} = {}", qi(k), json_literal(v)));
    }
    let wh = compile_conds(&q.conds, "t")?;
    if wh.is_empty() {
        return Err(ApiError::filters_required());
    }
    let mut sql = format!(
        "UPDATE {}.{} AS \"t\" SET {} WHERE {}",
        qi(&t.schema),
        qi(&t.name),
        sets.join(", "),
        wh
    );
    if wants_rows {
        sql.push_str(" RETURNING to_json(\"t\") AS \"row\"");
    }
    Ok(MutationSql { sql, wants_rows })
}

pub fn compile_delete(
    t: &TableInfo,
    q: &Query,
    wants_rows: bool,
) -> Result<MutationSql, ApiError> {
    let wh = compile_conds(&q.conds, "t")?;
    if wh.is_empty() {
        return Err(ApiError::filters_required());
    }
    let mut sql = format!(
        "DELETE FROM {}.{} AS \"t\" WHERE {}",
        qi(&t.schema),
        qi(&t.name),
        wh
    );
    if wants_rows {
        sql.push_str(" RETURNING to_json(\"t\") AS \"row\"");
    }
    Ok(MutationSql { sql, wants_rows })
}

// ---------- RPC ----------

pub struct RpcSql {
    pub sql: String,
    pub wants_rows: bool,
    pub scalar_text: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn compile_rpc(
    _cache: &SchemaCache,
    f: &FnInfo,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<RpcSql, ApiError> {
    // map provided args into declared arg positions; skip args with defaults
    let mut parts: Vec<String> = Vec::new();
    for name in &f.arg_names {
        match args.get(name) {
            Some(v) => {
                let lit = match v {
                    serde_json::Value::String(s) => ql(s),
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => v.to_string(),
                    serde_json::Value::Null => "NULL".to_string(),
                    other => ql(&other.to_string()),
                };
                parts.push(format!("{} => {}", qi(name), lit));
            }
            None => { /* omitted: falls back to the function default */ }
        }
    }

    let call = format!(
        "{}.{}({})",
        qi(&f.schema),
        qi(&f.name),
        parts.join(", ")
    );

    let sql = match &f.ret_kind {
        RetKind::Void => format!("SELECT {}", call),
        RetKind::Scalar(tyname) => {
            if tyname == "text" || tyname == "character varying" || tyname.starts_with("character") {
                format!("SELECT {}::text AS \"row\"", call)
            } else {
                format!("SELECT to_json({}) AS \"row\"", call)
            }
        }
        RetKind::Composite(_, _) | RetKind::SetOf(_, _) | RetKind::Table => {
            format!(
                "SELECT coalesce(json_agg(\"s\".\"row\"), '[]'::json) AS \"row\" \
                 FROM (SELECT to_json(\"r\") AS \"row\" FROM {} \"r\") \"s\"",
                call
            )
        }
    };
    Ok(RpcSql {
        sql,
        wants_rows: f.ret_kind != RetKind::Void,
        scalar_text: matches!(&f.ret_kind, RetKind::Scalar(t) if t == "text" || t.starts_with("character") || t == "character varying"),
    })
}

// for text_literal re-export
#[allow(unused_imports)]
use crate::sql::text_literal as _tl;
