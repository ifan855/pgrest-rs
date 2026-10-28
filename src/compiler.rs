// SQL compiler: read queries with resource embedding, aggregates and count.
// Everything is compiled to statements executed in ONE round trip.
use crate::ast::*;
use crate::error::ApiError;
use crate::schema::{SchemaCache, TableInfo};
use crate::sql::{qi, ql};

const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CountMode {
    None,
    Exact,
    Estimated,
}

pub struct ReadSql {
    pub count_sql: Option<String>,
    pub rows_sql: String,
}

// ---------- relationship resolution ----------

enum JoinKind {
    /// base is the child side: embed the parent row
    Parent { child_cols: Vec<String>, parent_cols: Vec<String> },
    /// target has an FK to base: embed children (or one if FK is unique)
    Children { child_cols: Vec<String>, parent_cols: Vec<String>, one: bool },
    /// many-to-many through a join table
    Many { via: String, base_fk: (Vec<String>, Vec<String>), target_fk: (Vec<String>, Vec<String>) },
    /// computed relationship: fn(base) returns the target rows
    Computed { fn_schema: String, fn_name: String, setof: bool },
}

struct Candidate {
    label: String,
    target: TableInfo,
    kind: JoinKind,
}

fn resolve_rel(
    cache: &SchemaCache,
    base: &TableInfo,
    node: &EmbedNode,
) -> Result<Candidate, ApiError> {
    let mut candidates: Vec<Candidate> = Vec::new();

    // 1. outgoing FKs: base -> target (parent embed)
    for fk in &base.fks {
        if fk.parent_table == node.rel {
            if let Some(target) = cache.table(&fk.parent_schema, &fk.parent_table) {
                candidates.push(Candidate {
                    label: fk.conname.clone(),
                    target: target.clone(),
                    kind: JoinKind::Parent {
                        child_cols: fk.child_cols.clone(),
                        parent_cols: fk.parent_cols.clone(),
                    },
                });
            }
        }
    }
    // 2. incoming FKs: target -> base (children embed); self-refs need a hint
    for (_, tinfo) in cache.tables.iter() {
        if tinfo.name != node.rel {
            continue;
        }
        if tinfo.schema == base.schema && tinfo.name == base.name && node.hint.is_none() {
            continue;
        }
        for fk in &tinfo.fks {
            if fk.parent_schema == base.schema && fk.parent_table == base.name {
                let one = tinfo.unique.iter().any(|u| u == &fk.child_cols);
                candidates.push(Candidate {
                    label: fk.conname.clone(),
                    target: tinfo.clone(),
                    kind: JoinKind::Children {
                        child_cols: fk.child_cols.clone(),
                        parent_cols: fk.parent_cols.clone(),
                        one,
                    },
                });
            }
        }
    }
    // 3. m2m through a join table
    for (_, jt) in cache.tables.iter() {
        if !jt.is_table() {
            continue;
        }
        if jt.name == base.name || jt.name == node.rel {
            continue;
        }
        let mut has_base = false;
        let mut base_fks: Vec<&crate::schema::Fk> = Vec::new();
        for f in &jt.fks {
            if f.parent_schema == base.schema && f.parent_table == base.name {
                has_base = true;
                base_fks.push(f);
            }
        }
        if !has_base {
            continue;
        }
        for fk_t in &jt.fks {
            if fk_t.parent_table == node.rel && fk_t.parent_schema == base.schema {
                if let Some(target) = cache.table(&fk_t.parent_schema, &fk_t.parent_table) {
                    for fk_b in base_fks.iter() {
                        if fk_b.conname == fk_t.conname {
                            continue;
                        }
                        candidates.push(Candidate {
                            label: format!("{}!{}", jt.name, fk_t.conname),
                            target: target.clone(),
                            kind: JoinKind::Many {
                                via: jt.name.clone(),
                                base_fk: (fk_b.child_cols.clone(), fk_b.parent_cols.clone()),
                                target_fk: (fk_t.child_cols.clone(), fk_t.parent_cols.clone()),
                            },
                        });
                    }
                }
            }
        }
    }

    // 4. computed relationships: fn(base) -> setof/composite target
    if let Some(rels) = cache.computed.get(&(base.schema.clone(), base.name.clone())) {
        for cr in rels {
            if cr.fn_name == node.rel {
                if let Some(target) = cache.table(&cr.ret_schema, &cr.ret_table) {
                    candidates.push(Candidate {
                        label: cr.fn_name.clone(),
                        target: target.clone(),
                        kind: JoinKind::Computed {
                            fn_schema: cr.fn_schema.clone(),
                            fn_name: cr.fn_name.clone(),
                            setof: cr.setof,
                        },
                    });
                }
            }
        }
    }

    if candidates.is_empty() {
        return Err(ApiError::new(
            404,
            "PGRST201",
            format!(
                "Could not find a relationship between '{}' and '{}' in the schema cache",
                base.name, node.rel
            ),
        )
        .with_details(format!(
            "Searched for a foreign key relationship, incoming relationship or many-to-many through a join table"
        )));
    }

    let chosen = if let Some(hint) = &node.hint {
        let filtered: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| match &c.kind {
                JoinKind::Many { via, .. } => via == hint || c.label.contains(hint),
                JoinKind::Computed { .. } => c.label == *hint,
                JoinKind::Parent { child_cols, .. } | JoinKind::Children { child_cols, .. } => {
                    c.label == *hint
                        || c.label.ends_with(&format!("_{}_fkey", hint))
                        || child_cols.first().map(|cc| cc == hint).unwrap_or(false)
                }
            })
            .collect();
        match filtered.len() {
            0 => {
                return Err(ApiError::new(
                    400,
                    "PGRST201",
                    format!(
                        "Could not find a relationship matching the hint '{}' between '{}' and '{}'",
                        hint, base.name, node.rel
                    ),
                ))
            }
            1 => vec![filtered[0].clone()],
            _ => {
                let names = filtered
                    .iter()
                    .map(|c| c.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ApiError::new(
                    300,
                    "PGRST201",
                    format!(
                        "Hint '{}' leaves multiple candidate relationships: {}",
                        hint, names
                    ),
                ));
            }
        }
    } else if candidates.len() > 1 {
        let names = candidates
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiError::new(
            300,
            "PGRST201",
            format!(
                "Could not choose the best candidate relationship between '{}' and '{}'. The candidates are: {}.",
                base.name, node.rel, names
            ),
        )
        .with_hint("Rename the relation or add a hint, e.g. films!hint"));
    } else {
        candidates
    };

    Ok(chosen[0].clone())
}

impl Clone for Candidate {
    fn clone(&self) -> Self {
        Candidate {
            label: self.label.clone(),
            target: self.target.clone(),
            kind: match &self.kind {
                JoinKind::Parent { child_cols, parent_cols } => JoinKind::Parent {
                    child_cols: child_cols.clone(),
                    parent_cols: parent_cols.clone(),
                },
                JoinKind::Children { child_cols, parent_cols, one } => JoinKind::Children {
                    child_cols: child_cols.clone(),
                    parent_cols: parent_cols.clone(),
                    one: *one,
                },
                JoinKind::Many { via, base_fk, target_fk } => JoinKind::Many {
                    via: via.clone(),
                    base_fk: (base_fk.0.clone(), base_fk.1.clone()),
                    target_fk: (target_fk.0.clone(), target_fk.1.clone()),
                },
                JoinKind::Computed { fn_schema, fn_name, setof } => JoinKind::Computed {
                    fn_schema: fn_schema.clone(),
                    fn_name: fn_name.clone(),
                    setof: *setof,
                },
            },
        }
    }
}

// ---------- embed SQL ----------

struct EmbedOut {
    select_exprs: Vec<String>,
    joins: Vec<String>,
    parent_exists: Vec<String>,
}

#[derive(Debug, Default)]
struct AliasGen {
    n: usize,
}

impl AliasGen {
    fn next(&mut self) -> usize {
        self.n += 1;
        self.n
    }
}



fn build_embed(
    cache: &SchemaCache,
    base: &TableInfo,
    base_alias: &str,
    sel: &Sel,
    node: &EmbedNode,
    aliases: &mut AliasGen,
    depth: usize,
) -> Result<EmbedOut, ApiError> {
    if depth > MAX_DEPTH {
        return Err(ApiError::parse("embed nesting too deep"));
    }
    let cand = resolve_rel(cache, base, node)?;
    let target = cand.target.clone();
    let en = aliases.next();
    let e_alias = format!("e{}", en);
    let inner_alias = format!("c{}", en);
    let jt_alias = format!("jt{}", en);
    let alias = sel.alias.clone().unwrap_or_else(|| node.rel.clone());

    // FROM + FK join condition for the lateral's inner query
    let (from_clause, fk_where): (String, String) = match &cand.kind {
        JoinKind::Computed { fn_schema, fn_name, .. } => {
            // function-in-FROM is implicitly LATERAL: `base_alias` is visible
            (
                format!(
                    "{}.{}({}) AS {}",
                    qi(fn_schema),
                    qi(fn_name),
                    qi(base_alias),
                    qi(&inner_alias)
                ),
                "true".to_string(),
            )
        }
        JoinKind::Parent { child_cols, parent_cols } => {
            let mut conds = Vec::new();
            for (cc, pc) in child_cols.iter().zip(parent_cols.iter()) {
                conds.push(format!(
                    "{}.{} IS NOT DISTINCT FROM {}.{}",
                    qi(&inner_alias),
                    qi(pc),
                    qi(base_alias),
                    qi(cc)
                ));
            }
            (
                format!(
                    "{}.{} AS {}",
                    qi(&target.schema),
                    qi(&target.name),
                    qi(&inner_alias)
                ),
                conds.join(" AND "),
            )
        }
        JoinKind::Children { child_cols, parent_cols, .. } => {
            let mut conds = Vec::new();
            for (cc, pc) in child_cols.iter().zip(parent_cols.iter()) {
                conds.push(format!(
                    "{}.{} IS NOT DISTINCT FROM {}.{}",
                    qi(&inner_alias),
                    qi(cc),
                    qi(base_alias),
                    qi(pc)
                ));
            }
            (
                format!(
                    "{}.{} AS {}",
                    qi(&target.schema),
                    qi(&target.name),
                    qi(&inner_alias)
                ),
                conds.join(" AND "),
            )
        }
        JoinKind::Many { base_fk, target_fk, .. } => {
            let mut on = Vec::new();
            for (jc, tc) in target_fk.0.iter().zip(target_fk.1.iter()) {
                on.push(format!(
                    "{}.{} IS NOT DISTINCT FROM {}.{}",
                    qi(&jt_alias),
                    qi(jc),
                    qi(&inner_alias),
                    qi(tc)
                ));
            }
            let mut wh = Vec::new();
            for (jc, bc) in base_fk.0.iter().zip(base_fk.1.iter()) {
                wh.push(format!(
                    "{}.{} IS NOT DISTINCT FROM {}.{}",
                    qi(&jt_alias),
                    qi(jc),
                    qi(base_alias),
                    qi(bc)
                ));
            }
            (
                format!(
                    "{sch}.{tbl} AS {ia} JOIN {sch2}.{jt} AS {ja} ON ({on})",
                    sch = qi(&target.schema),
                    tbl = qi(&target.name),
                    ia = qi(&inner_alias),
                    sch2 = qi(&base.schema),
                    jt = qi(match &cand.kind { JoinKind::Many { via, .. } => via, _ => unreachable!() }),
                    ja = qi(&jt_alias),
                    on = on.join(" AND ")
                ),
                wh.join(" AND "),
            )
        }
    };

    // aggregate-only embed: rel(count) / rel(sum(col))
    let agg_sels: Vec<&Sel> = node.select.iter().filter(|s| s.agg.is_some()).collect();
    if !agg_sels.is_empty() && agg_sels.len() != node.select.len() {
        // PostgREST does not allow mixing aggregates with plain columns inside an embed
        return Err(ApiError::parse(format!(
            "cannot mix aggregates with regular columns in embed '{}'",
            node.rel
        )));
    }
    if !node.select.is_empty() && agg_sels.len() == node.select.len() {
        let mut aggs: Vec<String> = Vec::new();
        for s in &agg_sels {
            let a = s.agg.as_ref().unwrap();
            let out_alias = s.alias.clone().unwrap_or_else(|| a.func.clone());
            let mut expr = match (a.func.as_str(), &a.arg) {
                ("count", None) => "count(*)::bigint".to_string(),
                ("count", Some(col)) if col == "*" => "count(*)::bigint".to_string(),
                ("count", Some(col)) => format!("count({})", colref(&inner_alias, col)),
                (f, Some(col)) => format!("{}({})", f, colref(&inner_alias, col)),
                (f, None) => {
                    return Err(ApiError::parse(format!("aggregate {}() requires a column", f)))
                }
            };
            if let Some(c) = &s.cast {
                expr = format!("({})::{}", expr, c);
            }
            aggs.push(format!("{} AS {}", expr, qi(&out_alias)));
        }
        let conds = compile_conds(&node.filters, &inner_alias)?;
        let wh = if conds.is_empty() {
            fk_where.clone()
        } else {
            format!("({}) AND ({})", fk_where, conds)
        };
        let join = format!(
            "LEFT JOIN LATERAL (SELECT coalesce(json_agg(\"a\"), '[]'::json) AS \"row\" \
             FROM (SELECT {aggs} FROM {from} WHERE {wh}) \"a\") {e} ON true",
            aggs = aggs.join(", "),
            from = from_clause,
            wh = wh,
            e = qi(&e_alias)
        );
        return Ok(EmbedOut {
            select_exprs: vec![format!("{}.\"row\" AS {}", qi(&e_alias), qi(&alias))],
            joins: vec![join],
            parent_exists: Vec::new(),
        });
    }

    // sub-embeds + projection
    let mut sub_joins: Vec<String> = Vec::new();
    let mut parent_exists: Vec<String> = Vec::new();
    let mut inner_select: Vec<String> = Vec::new();
    let plain_sels: Vec<&Sel> = node
        .select
        .iter()
        .filter(|s| s.agg.is_none() && s.embed.is_none())
        .collect();
    let has_embeds = node.select.iter().any(|s| s.embed.is_some());

    if node.select.is_empty() {
        inner_select.push(format!("{}.*", qi(&inner_alias)));
    } else {
        for s in &plain_sels {
            inner_select.push(project_col(&inner_alias, s)?);
        }
        if plain_sels.is_empty() && !has_embeds {
            inner_select.push(format!("{}.*", qi(&inner_alias)));
        }
    }
    for s in &node.select {
        if let Some(subnode) = &s.embed {
            let out = build_embed(cache, &target, &inner_alias, s, subnode, aliases, depth + 1)?;
            inner_select.extend(out.select_exprs.clone());
            sub_joins.extend(out.joins.clone());
            parent_exists.extend(out.parent_exists.clone());
        }
    }

    // to-one embeds: FK parents (filters lift to root EXISTS) and computed
    // scalar-composite returns (filters apply inside the lateral)
    let to_one = matches!(&cand.kind, JoinKind::Parent { .. } | JoinKind::Computed { setof: false, .. });
    if let JoinKind::Parent { child_cols, parent_cols } = &cand.kind {
        if !node.filters.is_empty() {
            let pex = format!("pex{}", en);
            let conds = compile_conds(&node.filters, &pex)?;
            let mut ex_conds = Vec::new();
            for (cc, pc) in child_cols.iter().zip(parent_cols.iter()) {
                ex_conds.push(format!(
                    "{}.{} IS NOT DISTINCT FROM {}.{}",
                    qi(&pex),
                    qi(pc),
                    qi(base_alias),
                    qi(cc)
                ));
            }
            parent_exists.push(format!(
                "EXISTS (SELECT 1 FROM {}.{} AS {} WHERE ({}) AND ({}))",
                qi(&target.schema),
                qi(&target.name),
                qi(&pex),
                ex_conds.join(" AND "),
                conds
            ));
        }
    }

    if to_one {
        let mut wh = fk_where.clone();
        if !node.filters.is_empty() && parent_exists.is_empty() {
            let conds = compile_conds(&node.filters, &inner_alias)?;
            if !conds.is_empty() {
                wh = format!("({}) AND ({})", wh, conds);
            }
        }
        // respect the requested projection (default: whole row) + sub-embeds
        let mut parent_proj: Vec<String> = Vec::new();
        if node.select.is_empty() {
            parent_proj.push(format!("{}.*", qi(&inner_alias)));
        } else {
            for s in &plain_sels {
                parent_proj.push(project_col(&inner_alias, s)?);
            }
            if plain_sels.is_empty() {
                parent_proj.push(format!("{}.*", qi(&inner_alias)));
            }
            for s in &node.select {
                if s.embed.is_some() {
                    if let Some(expr) = inner_select.iter().rev().find(|e| e.ends_with(&format!("AS {}", qi(&s.name)))) {
                        parent_proj.push(expr.clone());
                    }
                }
            }
        }
        let join_type = if node.inner { "INNER" } else { "LEFT" };
        let join = format!(
            "{jt} JOIN LATERAL (SELECT to_json(\"x\") AS \"row\" FROM (SELECT {proj} FROM {from}{subjoins} WHERE {wh} LIMIT 1) \"x\") {e} ON true",
            jt = join_type,
            proj = parent_proj.join(", "),
            from = from_clause,
            subjoins = if sub_joins.is_empty() { String::new() } else { format!(" {}", sub_joins.join(" ")) },
            wh = wh,
            e = qi(&e_alias)
        );
        return Ok(EmbedOut {
            select_exprs: vec![format!("{}.\"row\" AS {}", qi(&e_alias), qi(&alias))],
            joins: vec![join],
            parent_exists,
        });
    }

    // to-many embeds
    let mut wh = fk_where.clone();
    let conds = compile_conds(&node.filters, &inner_alias)?;
    if !conds.is_empty() {
        wh = format!("({}) AND ({})", wh, conds);
    }
    let mut tail = String::new();
    if !node.order.is_empty() {
        tail.push_str(&format!(
            " ORDER BY {}",
            node.order
                .iter()
                .map(|o| compile_order_item(o, &inner_alias))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(l) = node.limit {
        tail.push_str(&format!(" LIMIT {}", l));
    }
    if let Some(o) = node.offset {
        tail.push_str(&format!(" OFFSET {}", o));
    }
    let join_type = if node.inner { "INNER" } else { "LEFT" };
    let agg_expr = if node.inner {
        "json_agg(\"s\".\"row\")".to_string()
    } else {
        "coalesce(json_agg(\"s\".\"row\"), '[]'::json)".to_string()
    };
    let on_clause = if node.inner {
        format!("{}.\"row\" IS NOT NULL", qi(&e_alias))
    } else {
        "true".to_string()
    };
    let join = format!(
        "{jt} JOIN LATERAL (SELECT {agg} AS \"row\" \
         FROM (SELECT to_json(\"x\") AS \"row\" FROM (SELECT {proj} FROM {from}{subjoins} WHERE {wh}{tail}) \"x\") \"s\") {e} ON {on}",
        jt = join_type,
        agg = agg_expr,
        proj = inner_select.join(", "),
        from = from_clause,
        subjoins = if sub_joins.is_empty() { String::new() } else { format!(" {}", sub_joins.join(" ")) },
        wh = wh,
        tail = tail,
        e = qi(&e_alias),
        on = on_clause
    );
    Ok(EmbedOut {
        select_exprs: vec![format!("{}.\"row\" AS {}", qi(&e_alias), qi(&alias))],
        joins: vec![join],
        parent_exists,
    })
}

fn colref(alias: &str, col: &str) -> String {
    for arrow in ["->>", "->"] {
        if let Some(pos) = col.find(arrow) {
            let (c, k) = col.split_at(pos);
            let key = k[arrow.len()..].trim().trim_matches('\'');
            return format!("{}.{}{}{}", qi(alias), qi(c), arrow, ql(key));
        }
    }
    format!("{}.{}", qi(alias), qi(col))
}

fn project_col(alias: &str, s: &Sel) -> Result<String, ApiError> {
    let expr = project_expr(alias, s)?;
    // `*` spreads every column of the source; `alias:*` behaves the same
    if s.name == "*" {
        return Ok(expr);
    }
    let out = s.alias.clone().unwrap_or_else(|| s.name.clone());
    Ok(format!("{} AS {}", expr, qi(&out)))
}

/// The bare SQL expression for a select item (no output alias).
fn project_expr(alias: &str, s: &Sel) -> Result<String, ApiError> {
    if s.name == "*" {
        return Ok(format!("{}.*", qi(alias)));
    }
    let mut expr = colref(alias, &s.name);
    if let Some((k, is_text)) = &s.json {
        expr = format!(
            "{}{}{}",
            format!("{}.{}", qi(alias), qi(&s.name)),
            if *is_text { "->>" } else { "->" },
            ql(k)
        );
    }
    if let Some(c) = &s.cast {
        expr = format!("({})::{}", expr, c);
    }
    Ok(expr)
}

fn compile_order_item(o: &OrderItem, alias: &str) -> String {
    let mut s = colref(alias, &o.col);
    s.push_str(match o.dir {
        Some(Dir::Desc) => " DESC",
        _ => " ASC",
    });
    if let Some(n) = o.nulls {
        s.push_str(match n {
            Nulls::First => " NULLS FIRST",
            Nulls::Last => " NULLS LAST",
        });
    }
    s
}

// ---------- condition compilation ----------

pub fn compile_conds(conds: &[Cond], alias: &str) -> Result<String, ApiError> {
    let mut parts = Vec::new();
    for c in conds {
        parts.push(compile_cond(c, alias)?);
    }
    Ok(parts.join(" AND "))
}

fn compile_cond(c: &Cond, alias: &str) -> Result<String, ApiError> {
    match c {
        Cond::Group { op, not, items } => {
            let inner = items
                .iter()
                .map(|i| compile_cond(i, alias))
                .collect::<Result<Vec<_>, _>>()?;
            if inner.is_empty() {
                return Ok("true".into());
            }
            let joined = match op {
                LogicOp::And => inner.join(" AND "),
                LogicOp::Or => inner.join(" OR "),
            };
            Ok(format!("{}({})", if *not { "NOT " } else { "" }, joined))
        }
        Cond::Leaf { col, op, val, not } => {
            let expr = leaf_sql(col, op, val, alias)?;
            Ok(format!("{}({})", if *not { "NOT " } else { "" }, expr))
        }
    }
}

fn leaf_sql(col: &str, op: &str, val: &str, alias: &str) -> Result<String, ApiError> {
    let left = colref(alias, col);
    let v = val.trim();
    // normalize operator: base + optional language in parens (fts(english)) or after a dot (fts.english)
    let (base, lang): (&str, Option<&str>) = if let Some(open) = op.find('(') {
        let b = &op[..open];
        let l = op[open + 1..].trim_end_matches(')');
        (b, Some(l))
    } else if let Some(dot) = op.find('.') {
        (&op[..dot], Some(&op[dot + 1..]))
    } else {
        (op, None)
    };

    let sql = match base {
        "eq" | "neq" => {
            if v.eq_ignore_ascii_case("null") {
                format!("{} IS {}NULL", left, if base == "eq" { "" } else { "NOT " })
            } else {
                format!("{} {} {}", left, if base == "eq" { "=" } else { "<>" }, ql(v))
            }
        }
        "gt" => format!("{} > {}", left, ql(v)),
        "gte" => format!("{} >= {}", left, ql(v)),
        "lt" => format!("{} < {}", left, ql(v)),
        "lte" => format!("{} <= {}", left, ql(v)),
        "like" => format!("{} LIKE {}", left, ql(&v.replace("*", "%"))),
        "ilike" => format!("{} ILIKE {}", left, ql(&v.replace("*", "%"))),
        "match" => format!("{} ~ {}", left, ql(v)),
        "imatch" => format!("{} ~* {}", left, ql(v)),
        "in" => {
            let inner = v.trim_start_matches('(').trim_end_matches(')');
            let items = crate::parser::split_top_commas(inner)
                .into_iter()
                .map(|s| {
                    let s = s.trim();
                    // PostgREST in-lists: bare values, 'single quoted' or "double quoted"
                    if s.len() >= 2
                        && ((s.starts_with('\'') && s.ends_with('\''))
                            || (s.starts_with('"') && s.ends_with('"')))
                    {
                        ql(&s[1..s.len() - 1])
                    } else {
                        ql(s)
                    }
                })
                .collect::<Vec<_>>();
            if items.is_empty() {
                "false".to_string()
            } else {
                format!("{} IN ({})", left, items.join(", "))
            }
        }
        "is" => match v.to_lowercase().as_str() {
            "null" => format!("{} IS NULL", left),
            "not_null" | "notnull" => format!("{} IS NOT NULL", left),
            "true" => format!("{} IS TRUE", left),
            "false" => format!("{} IS FALSE", left),
            "unknown" => format!("{} IS UNKNOWN", left),
            other => return Err(ApiError::parse(format!("invalid 'is' value '{}'", other))),
        },
        "isdistinct" => format!("{} IS DISTINCT FROM {}", left, ql(v)),
        "cs" => format!("{} @> {}", left, ql(v)),
        "cd" => format!("{} <@ {}", left, ql(v)),
        "ov" => format!("{} && {}", left, ql(v)),
        "sl" => format!("{} << {}", left, ql(v)),
        "sr" => format!("{} >> {}", left, ql(v)),
        "nxl" => format!("{} &< {}", left, ql(v)),
        "nxr" => format!("{} &> {}", left, ql(v)),
        "adj" => format!("{} -|- {}", left, ql(v)),
        "fts" | "plfts" | "phfts" | "websearch" => {
            let fnname = match base {
                "fts" => "to_tsquery",
                "plfts" => "plainto_tsquery",
                "phfts" => "phraseto_tsquery",
                _ => "websearch_to_tsquery",
            };
            let call = match lang {
                Some(l) if !l.is_empty() => format!("{}({}, {})", fnname, ql(l), ql(v)),
                _ => format!("{}({})", fnname, ql(v)),
            };
            format!("{} @@ {}", left, call)
        }
        other => return Err(ApiError::parse(format!("unknown operator '{}'", other))),
    };
    Ok(sql)
}

// ---------- mutation-return projection & RPC shaping ----------

/// Outer SELECT over a `pgrest_src` CTE (mutation RETURNING *), applying the
/// requested `select=` projection and any resource embeds.
pub fn compile_return_read(
    cache: &SchemaCache,
    t: &TableInfo,
    select: &[Sel],
) -> Result<String, ApiError> {
    if select.is_empty() {
        return Ok("SELECT to_json(\"t0\") AS \"row\" FROM \"pgrest_src\" AS \"t0\"".to_string());
    }
    let mut aliases = AliasGen::default();
    let mut proj: Vec<String> = Vec::new();
    let mut joins: Vec<String> = Vec::new();
    for s in select {
        if let Some(node) = &s.embed {
            let eo = build_embed(cache, t, "t0", s, node, &mut aliases, 0)?;
            proj.extend(eo.select_exprs);
            joins.extend(eo.joins);
        } else {
            proj.push(project_col("t0", s)?);
        }
    }
    Ok(format!(
        "SELECT to_json(\"x\") AS \"row\" FROM (SELECT {proj} FROM \"pgrest_src\" AS \"t0\"{joins}) \"x\"",
        proj = proj.join(", "),
        joins = if joins.is_empty() { String::new() } else { format!(" {}", joins.join(" ")) }
    ))
}

/// Read-shaping parameters for setof-returning RPC calls.
#[derive(Default)]
pub struct RpcShape {
    /// The SETOF target table (enables embeds + typed projection). `None` for
    /// TABLE(...) record types, where only plain column selection works.
    pub base: Option<TableInfo>,
    pub select: Vec<Sel>,
    pub conds: Vec<Cond>,
    pub order: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Shape a setof-returning function call: per-row JSON docs with projection,
/// embeds, filters, order and limit/offset applied.
pub fn compile_rpc_shaping(
    cache: &SchemaCache,
    call: &str,
    sh: &RpcShape,
) -> Result<String, ApiError> {
    let alias = "r";
    let from = format!("{} AS {}", call, qi(alias));
    let any_agg = sh.select.iter().any(|s| s.agg.is_some());
    if any_agg {
        if sh.select.iter().any(|s| s.embed.is_some()) {
            return Err(ApiError::parse(
                "cannot mix aggregate functions with resource embeds in select",
            ));
        }
        let mut exprs: Vec<String> = Vec::new();
        let mut group_by: Vec<String> = Vec::new();
        for s in &sh.select {
            if s.agg.is_some() {
                exprs.push(agg_expr(alias, s)?);
            } else {
                let bare = project_expr(alias, s)?;
                if bare.ends_with(".*") {
                    return Err(ApiError::parse("'*' cannot be combined with aggregates"));
                }
                group_by.push(bare.clone());
                exprs.push(format!("{} AS {}", bare, qi(&s.alias.clone().unwrap_or_else(|| s.name.clone()))));
            }
        }
        let wh = compile_conds(&sh.conds, alias)?;
        let mut tail = String::new();
        if !group_by.is_empty() {
            tail.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
        }
        if !sh.order.is_empty() {
            tail.push_str(&format!(
                " ORDER BY {}",
                sh.order.iter().map(|o| compile_order_item(o, alias)).collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(l) = sh.limit {
            tail.push_str(&format!(" LIMIT {}", l));
        }
        if let Some(o) = sh.offset {
            tail.push_str(&format!(" OFFSET {}", o));
        }
        return Ok(format!(
            "SELECT to_json(\"x\") AS \"row\" FROM (SELECT {exprs} FROM {from}{wh}{tail}) \"x\"",
            exprs = exprs.join(", "),
            from = from,
            wh = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh) },
            tail = tail
        ));
    }

    let mut aliases = AliasGen::default();
    let mut proj: Vec<String> = Vec::new();
    let mut joins: Vec<String> = Vec::new();
    if sh.select.is_empty() {
        proj.push(format!("{}.*", qi(alias)));
    } else {
        for s in &sh.select {
            if let Some(node) = &s.embed {
                let base = sh.base.as_ref().ok_or_else(|| {
                    ApiError::parse("cannot embed into a TABLE() function result")
                })?;
                let eo = build_embed(cache, base, alias, s, node, &mut aliases, 0)?;
                proj.extend(eo.select_exprs);
                joins.extend(eo.joins);
            } else {
                proj.push(project_col(alias, s)?);
            }
        }
    }
    let wh = compile_conds(&sh.conds, alias)?;
    let mut tail = String::new();
    if !sh.order.is_empty() {
        tail.push_str(&format!(
            " ORDER BY {}",
            sh.order.iter().map(|o| compile_order_item(o, alias)).collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(l) = sh.limit {
        tail.push_str(&format!(" LIMIT {}", l));
    }
    if let Some(o) = sh.offset {
        tail.push_str(&format!(" OFFSET {}", o));
    }
    Ok(format!(
        "SELECT to_json(\"x\") AS \"row\" FROM (SELECT {proj} FROM {from}{joins}{wh}{tail}) \"x\"",
        proj = proj.join(", "),
        from = from,
        joins = if joins.is_empty() { String::new() } else { format!(" {}", joins.join(" ")) },
        wh = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh) },
        tail = tail
    ))
}

/// Render the aggregate SQL expression for one select item (no alias).
fn agg_expr_sql(alias: &str, a: &Agg) -> Result<String, ApiError> {
    let e = match (a.func.as_str(), &a.arg) {
        ("count", None) => "count(*)::bigint".to_string(),
        ("count", Some(col)) if col == "*" => "count(*)::bigint".to_string(),
        ("count", Some(col)) => format!("count({})", colref(alias, col)),
        (f, Some(col)) => format!("{}({})", f, colref(alias, col)),
        (f, None) => return Err(ApiError::parse(format!("aggregate {}() requires a column", f))),
    };
    Ok(e)
}

/// Render one aggregate select item (`count()`, `sum(col)`, aliases, casts).
fn agg_expr(alias: &str, s: &Sel) -> Result<String, ApiError> {
    let a = s.agg.as_ref().ok_or_else(|| ApiError::parse("not an aggregate"))?;
    let out = s.alias.clone().unwrap_or_else(|| a.func.clone());
    let mut e = agg_expr_sql(alias, a)?;
    if let Some(c) = &s.cast {
        e = format!("({})::{}", e, c);
    }
    Ok(format!("{} AS {}", e, qi(&out)))
}

// ---------- top-level read ----------

pub fn compile_read(
    cache: &SchemaCache,
    t: &TableInfo,
    q: &Query,
    count: CountMode,
    max_rows: Option<i64>,
) -> Result<ReadSql, ApiError> {
    let has_agg = q.select.iter().any(|s| s.agg.is_some());
    if has_agg {
        if q.select.iter().any(|s| s.embed.is_some()) {
            return Err(ApiError::parse(
                "cannot mix aggregate functions with resource embeds in select",
            ));
        }
        return compile_aggregate(t, q);
    }

    let mut aliases = AliasGen::default();
    let root_alias = "t0";
    let mut out = EmbedOut { select_exprs: Vec::new(), joins: Vec::new(), parent_exists: Vec::new() };
    let mut proj: Vec<String> = Vec::new();

    if q.select.is_empty() {
        proj.push(format!("{}.*", qi(root_alias)));
    } else {
        for s in &q.select {
            if let Some(node) = &s.embed {
                let eo = build_embed(cache, t, root_alias, s, node, &mut aliases, 0)?;
                proj.extend(eo.select_exprs.clone());
                out.joins.extend(eo.joins.clone());
                out.parent_exists.extend(eo.parent_exists.clone());
            } else {
                proj.push(project_col(root_alias, s)?);
            }
        }
    }

    let mut wh = compile_conds(&q.conds, root_alias)?;
    for ex in &out.parent_exists {
        wh = if wh.is_empty() { ex.clone() } else { format!("({}) AND ({})", wh, ex) };
    }

    let from = format!("{}.{} AS {}", qi(&t.schema), qi(&t.name), qi(root_alias));
    let joins = if out.joins.is_empty() { String::new() } else { format!(" {}", out.joins.join(" ")) };

    let mut tail = String::new();
    if !q.order.is_empty() {
        tail.push_str(&format!(
            " ORDER BY {}",
            q.order
                .iter()
                .map(|o| compile_order_item(o, root_alias))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut limit = q.limit;
    if let Some(max) = max_rows {
        if limit.is_none() || limit.unwrap() > max {
            limit = Some(max);
        }
    }
    if let Some(l) = limit {
        tail.push_str(&format!(" LIMIT {}", l));
    }
    if let Some(o) = q.offset {
        tail.push_str(&format!(" OFFSET {}", o));
    }

    let rows_sql = format!(
        "WITH pgrst_source AS (SELECT to_json(\"x\") AS \"row\" FROM (SELECT {proj} FROM {from}{joins}{wh}{tail}) \"x\") SELECT \"row\" FROM pgrst_source",
        proj = proj.join(", "),
        from = from,
        joins = joins,
        wh = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh) },
        tail = tail
    );

    let count_sql = match count {
        CountMode::None => None,
        CountMode::Exact => {
            let inner_joins = out
                .joins
                .iter()
                .filter(|j| j.starts_with("INNER"))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let jsql = if inner_joins.is_empty() { String::new() } else { format!(" {}", inner_joins) };
            Some(format!(
                "SELECT count(*)::bigint AS total FROM (SELECT 1 FROM {from}{jsql}{wh}) \"c\"",
                from = from,
                jsql = jsql,
                wh = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh) }
            ))
        }
        CountMode::Estimated => {
            // PostgREST "planned" count: row estimate from EXPLAIN of the
            // read query itself (limit/offset stripped, inner joins kept).
            let inner_joins = out
                .joins
                .iter()
                .filter(|j| j.starts_with("INNER"))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let jsql = if inner_joins.is_empty() { String::new() } else { format!(" {}", inner_joins) };
            Some(format!(
                "EXPLAIN (FORMAT JSON) SELECT 1 FROM {from}{jsql}{wh}",
                from = from,
                jsql = jsql,
                wh = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh) }
            ))
        }
    };

    Ok(ReadSql { count_sql, rows_sql })
}

fn compile_aggregate(t: &TableInfo, q: &Query) -> Result<ReadSql, ApiError> {
    let alias = "t0";
    let mut exprs: Vec<String> = Vec::new();
    let mut group_by: Vec<String> = Vec::new();

    // plain columns alongside aggregates: PostgREST auto-groups by them.
    // every output column is to_jsonb-wrapped so the JSON types survive the
    // text protocol (numeric vs ::text vs boolean)
    for s in &q.select {
        if s.embed.is_some() {
            continue; // already rejected in compile_read
        }
        if s.agg.is_some() {
            let a = s.agg.as_ref().unwrap();
            let out = s.alias.clone().unwrap_or_else(|| a.func.clone());
            let mut e = agg_expr_sql(alias, a)?;
            if let Some(c) = &s.cast {
                e = format!("({})::{}", e, c);
            }
            exprs.push(format!("to_jsonb({}) AS {}", e, qi(&out)));
        } else {
            let bare = project_expr(alias, s)?;
            if bare.ends_with(".*") {
                return Err(ApiError::parse("'*' cannot be combined with aggregates"));
            }
            group_by.push(bare.clone());
            exprs.push(format!("to_jsonb({}) AS {}", bare, qi(&s.alias.clone().unwrap_or_else(|| s.name.clone()))));
        }
    }

    let wh = compile_conds(&q.conds, alias)?;
    let mut tail = String::new();
    if !group_by.is_empty() {
        tail.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
    }
    if !q.order.is_empty() {
        tail.push_str(&format!(
            " ORDER BY {}",
            q.order
                .iter()
                .map(|o| compile_order_item(o, alias))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(l) = q.limit {
        tail.push_str(&format!(" LIMIT {}", l));
    }
    if let Some(o) = q.offset {
        tail.push_str(&format!(" OFFSET {}", o));
    }
    let rows_sql = format!(
        "SELECT {} FROM {}.{} AS {}{}{}",
        exprs.join(", "),
        qi(&t.schema),
        qi(&t.name),
        qi(alias),
        if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh) },
        tail
    );
    Ok(ReadSql { count_sql: None, rows_sql })
}
