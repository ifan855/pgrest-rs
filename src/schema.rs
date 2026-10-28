// Schema cache: one introspection pass over pg_catalog, rebuilt on demand and
// hot-reloaded via `NOTIFY pgrst, 'reload schema'`.
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ColInfo {
    pub name: String,
    pub type_name: String, // e.g. integer, text, tsvector, numeric
    pub nullable: bool,
    pub has_default: bool,
    pub generated: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Fk {
    pub conname: String,
    pub child_schema: String,
    pub child_table: String,
    pub child_cols: Vec<String>,
    pub parent_schema: String,
    pub parent_table: String,
    pub parent_cols: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: char, // 'r' table, 'v' view, 'm' matview, 'p' partitioned
    pub columns: Vec<ColInfo>,
    pub pks: Vec<String>,
    pub fks: Vec<Fk>,        // outgoing (this table is child side)
    pub unique: Vec<Vec<String>>, // unique constraint col sets (incl. PK)
    pub comment: Option<String>,
}

impl TableInfo {
    pub fn col(&self, name: &str) -> Option<&ColInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
    pub fn is_table(&self) -> bool {
        self.kind == 'r' || self.kind == 'p'
    }
}

#[derive(Debug, Clone)]
pub struct FnInfo {
    pub schema: String,
    pub name: String,
    pub arg_names: Vec<String>, // IN/INOUT/VARIADIC names in identity order
    pub required: usize,        // args before the first DEFAULT (must be supplied)
    pub ret_kind: RetKind,
    pub out_cols: Vec<String>, // column names for TABLE(...) returns
    pub volatile: bool,
    pub is_procedure: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RetKind {
    Void,
    Scalar(String),   // type name
    Composite(String, String), // schema.table
    SetOfScalar(String),       // SETOF <scalar type>
    SetOf(String, String),     // SETOF schema.table
    Table, // returns table(...) -- setof record via OUT cols
}

/// A computed relationship: a function taking exactly one table-type argument
/// whose result is a table/setof-table, exposed as an embed relation.
#[derive(Debug, Clone)]
pub struct ComputedRel {
    pub fn_schema: String,
    pub fn_name: String,
    pub ret_schema: String,
    pub ret_table: String,
    pub setof: bool,
}

#[derive(Debug, Default)]
pub struct SchemaCache {
    pub tables: HashMap<(String, String), TableInfo>,
    pub functions: HashMap<(String, String), Vec<FnInfo>>, // overloads grouped by name
    pub computed: HashMap<(String, String), Vec<ComputedRel>>, // keyed by (arg schema, arg table)
    pub enums: HashMap<(String, String), Vec<String>>,
    pub schemas: Vec<String>,
}

impl SchemaCache {
    pub fn table(&self, schema: &str, name: &str) -> Option<&TableInfo> {
        self.tables.get(&(schema.to_string(), name.to_string()))
    }

    pub fn find_table(&self, schemas: &[String], name: &str) -> Result<&TableInfo, FindErr> {
        let mut hits: Vec<&TableInfo> = Vec::new();
        for s in schemas {
            if let Some(t) = self.table(s, name) {
                hits.push(t);
            }
        }
        match hits.len() {
            0 => Err(FindErr::Missing),
            1 => Ok(hits[0]),
            _ => Err(FindErr::Ambiguous),
        }
    }

    pub fn function(&self, schema: &str, name: &str) -> Option<&Vec<FnInfo>> {
        self.functions.get(&(schema.to_string(), name.to_string()))
    }

    /// All overloads of `name` across the exposed schemas, grouped per schema.
    /// Errors when the name is missing or defined in more than one schema.
    pub fn find_function(&self, schemas: &[String], name: &str) -> Result<Vec<&FnInfo>, FindErr> {
        let mut hits: Vec<&FnInfo> = Vec::new();
        let mut n_schemas = 0usize;
        for s in schemas {
            if let Some(fs) = self.function(s, name) {
                n_schemas += 1;
                for f in fs {
                    hits.push(f);
                }
            }
        }
        match (hits.len(), n_schemas) {
            (0, _) => Err(FindErr::Missing),
            (_, 0) => Err(FindErr::Missing),
            (_, 1) => Ok(hits),
            _ => Err(FindErr::Ambiguous),
        }
    }
}

pub enum FindErr {
    Missing,
    Ambiguous,
}

const INTROSPECT_SQL: &str = r#"
SELECT n.nspname, c.relname, c.relkind, a.attname,
       format_type(a.atttypid, a.atttypmod) AS coltype,
       a.attnotnull, (a.attgenerated <> '') AS generated,
       pg_get_expr(ad.adbin, ad.adrelid) AS defaultexpr,
       col_description(c.oid, a.attnum) AS comment
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
LEFT JOIN pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum
WHERE n.nspname = ANY({schemas}) AND c.relkind IN ('r','v','m','p','f')
ORDER BY n.nspname, c.relname, a.attnum
"#;

const CONSTRAINT_SQL: &str = r#"
SELECT n.nspname AS child_schema, cc.relname AS child_table, con.conname,
       con.contype, con.conkey, con.confkey,
       cn.nspname AS parent_schema, cp.relname AS parent_table,
       (SELECT array_agg(a.attname ORDER BY ord)::text
          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum) AS child_cols,
       (SELECT array_agg(a.attname ORDER BY ord)::text
          FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum) AS parent_cols
FROM pg_constraint con
JOIN pg_class cc ON cc.oid = con.conrelid
JOIN pg_namespace n ON n.oid = cc.relnamespace
LEFT JOIN pg_class cp ON cp.oid = con.confrelid
LEFT JOIN pg_namespace cn ON cn.oid = cp.relnamespace
WHERE n.nspname = ANY({schemas}) AND con.contype IN ('f','u','p')
"#;

const FUNCTION_SQL: &str = r#"
SELECT n.nspname, p.proname, p.prokind, p.provolatile,
       pg_get_function_identity_arguments(p.oid) AS args,
       pg_get_function_result(p.oid) AS ret,
       pg_get_function_arguments(p.oid) AS all_args,
       p.proargnames
FROM pg_proc p
JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = ANY({schemas}) AND p.prokind IN ('f','p')
"#;

const ENUM_SQL: &str = r#"
SELECT n.nspname, t.typname, array_agg(e.enumlabel ORDER BY e.enumsortorder)::text AS labels
FROM pg_type t
JOIN pg_enum e ON e.enumtypid = t.oid
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = ANY({schemas})
GROUP BY 1,2
"#;

// Computed relationships: functions with EXACTLY one argument whose type is a
// table (composite) in the exposed schemas, returning a table or setof table
// in the exposed schemas. Exposed as embed relations named after the function.
// The argument type is the table's ROWTYPE oid, routed through pg_type.typrelid.
const COMPUTED_SQL: &str = r#"
SELECT fn.nspname AS fn_schema, p.proname,
       an.nspname AS arg_schema, at.relname AS arg_table,
       rn.nspname AS ret_schema, rt.relname AS ret_table,
       (pg_get_function_result(p.oid) LIKE 'SETOF %') AS setof
FROM pg_proc p
JOIN pg_namespace fn ON fn.oid = p.pronamespace
JOIN pg_type argty ON argty.oid = p.proargtypes[0]
JOIN pg_class at ON at.oid = argty.typrelid
JOIN pg_namespace an ON an.oid = at.relnamespace
JOIN pg_class rt ON rt.oid = COALESCE(to_regclass(regexp_replace(pg_get_function_result(p.oid), '^SETOF ', '')), 0)
JOIN pg_namespace rn ON rn.oid = rt.relnamespace
WHERE fn.nspname = ANY({schemas})
  AND an.nspname = ANY({schemas})
  AND rn.nspname = ANY({schemas})
  AND at.relkind IN ('r','v','m','p','f')
  AND rt.relkind IN ('r','v','m','p','f')
  AND cardinality(p.proargtypes) = 1
  AND p.prokind = 'f'
"#;


fn schema_list_sql(schemas: &[String]) -> String {
    format!(
        "ARRAY[{}]",
        schemas
            .iter()
            .map(|s| crate::sql::ql(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn parse_pg_array(s: Option<&str>) -> Vec<String> {
    // handles simple text[] arrays: {a,b} or {"a b",c}
    let Some(s) = s else { return Vec::new() };
    let s = s.trim();
    if !s.starts_with('{') {
        return Vec::new();
    }
    let inner = &s[1..s.len().saturating_sub(1)];
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        if in_q {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    cur.push('"');
                    it.next();
                } else {
                    in_q = false;
                    out.push(std::mem::take(&mut cur));
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_q = true,
                ',' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub async fn load(
    pool: &deadpool_postgres::Pool,
    schemas: &[String],
) -> Result<SchemaCache, crate::error::ApiError> {
    use crate::error::ApiError;
    let mut cache = SchemaCache {
        schemas: schemas.to_vec(),
        ..Default::default()
    };
    let client = pool.get().await.map_err(ApiError::from_pool)?;

    // tables + columns
    let list = schema_list_sql(schemas);
    let rows = client
        .query(&INTROSPECT_SQL.replace("{schemas}", &list), &[])
        .await
        .map_err(ApiError::from_db)?;
    for r in rows {
        let schema: String = r.get(0);
        let tname: String = r.get(1);
        let kind: i8 = r.get(2);
        let key = (schema.clone(), tname.clone());
        let t = cache
            .tables
            .entry(key)
            .or_insert_with(|| TableInfo {
                schema: schema.clone(),
                name: tname.clone(),
                kind: kind as u8 as char,
                ..Default::default()
            });
        if let Some(attname) = r.get::<_, Option<String>>(3) {
            let defaultexpr: Option<String> = r.get(7);
            t.columns.push(ColInfo {
                name: attname,
                type_name: r.get(4),
                nullable: !r.get::<_, bool>(5),
                has_default: defaultexpr.is_some(),
                generated: r.get(6),
                comment: r.get(8),
            });
        }
    }

    // constraints
    let rows = client
        .query(&CONSTRAINT_SQL.replace("{schemas}", &list), &[])
        .await
        .map_err(ApiError::from_db)?;
    for r in rows {
        let child_schema: String = r.get(0);
        let child_table: String = r.get(1);
        let conname: String = r.get(2);
        let contype: i8 = r.get(3);
        let child_cols: Vec<String> = parse_pg_array(r.get(8));
        match contype as u8 as char {
            'p' => {
                if let Some(t) = cache.tables.get_mut(&(child_schema.clone(), child_table.clone())) {
                    t.pks = child_cols.clone();
                    t.unique.push(child_cols);
                }
            }
            'u' => {
                if let Some(t) = cache.tables.get_mut(&(child_schema.clone(), child_table.clone())) {
                    t.unique.push(child_cols);
                }
            }
            'f' => {
                let parent_schema: String = r.get(6);
                let parent_table: String = r.get(7);
                let parent_cols: Vec<String> = parse_pg_array(r.get(9));
                let fk = Fk {
                    conname,
                    child_schema: child_schema.clone(),
                    child_table: child_table.clone(),
                    child_cols,
                    parent_schema,
                    parent_table,
                    parent_cols,
                };
                if let Some(t) = cache.tables.get_mut(&(child_schema, child_table)) {
                    t.fks.push(fk);
                }
            }
            _ => {}
        }
    }

    // functions
    let rows = client
        .query(&FUNCTION_SQL.replace("{schemas}", &list), &[])
        .await
        .map_err(ApiError::from_db)?;
    for r in rows {
        let schema: String = r.get(0);
        let name: String = r.get(1);
        let prokind: i8 = r.get(2);
        let provolatile: i8 = r.get(3);
        let args: String = r.get(4);
        let ret: Option<String> = r.get(5); // NULL for procedures
        let all_args: String = r.get(6); // includes DEFAULT clauses
        let proargnames: Option<Vec<String>> = r.get(7);

        // identity args: "a int, b text DEFAULT 1" -> names a, b (skip OUTs: identity args are IN/INOUT/VARIADIC only)
        let mut arg_names: Vec<String> = Vec::new();
        for part in args.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            // optional mode prefix IN/OUT/INOUT/VARIADIC
            let mut segs = part.split_whitespace();
            let mut first = segs.next().unwrap_or("");
            if matches!(first, "IN" | "OUT" | "INOUT" | "VARIADIC") {
                first = segs.next().unwrap_or("");
            }
            let fname = first.split('·').next().unwrap_or(first);
            if !fname.is_empty() && fname.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false) {
                arg_names.push(fname.to_string());
            }
        }
        if let Some(names) = proargnames {
            // prefer proargnames when they align (identity order)
            if names.len() >= arg_names.len() && !names.is_empty() {
                arg_names = names.iter().take(arg_names.len()).cloned().collect();
            }
        }
        // identity args omit DEFAULT clauses; count them from the full
        // signature instead (defaults are always trailing in PostgreSQL)
        let (total_args, defaults) = scan_args_and_defaults(&all_args);
        let required = arg_names
            .len()
            .saturating_sub(defaults)
            .min(arg_names.len());
        let _ = total_args;

        let ret = ret.unwrap_or_default();
        let (ret_kind, out_cols) = if ret.is_empty() {
            (RetKind::Void, Vec::new())
        } else if ret == "void" {
            (RetKind::Void, Vec::new())
        } else if let Some(inner) = ret.strip_prefix("TABLE(") {
            // TABLE(a numeric, b bigint) -- setof record; OUT column names are usable
            let inner = inner.strip_suffix(')').unwrap_or(inner);
            let cols = split_top_level_commas(inner)
                .into_iter()
                .filter_map(|c| {
                    let c = c.trim();
                    if c.is_empty() {
                        None
                    } else {
                        // name type / mode name type
                        let first = c.split_whitespace().next().unwrap_or(c);
                        if matches!(first.to_uppercase().as_str(), "IN" | "OUT" | "INOUT" | "VARIADIC") {
                            c.split_whitespace().nth(1).map(|s| s.to_string())
                        } else {
                            Some(first.to_string())
                        }
                    }
                })
                .collect();
            (RetKind::Table, cols)
        } else if let Some(base) = ret.strip_prefix("SETOF ") {
            if let Some((s, t)) = split_qualified(&format!("SETOF {}", base)) {
                (RetKind::SetOf(s, t), Vec::new())
            } else {
                (RetKind::SetOfScalar(base.trim_matches('"').to_string()), Vec::new())
            }
        } else if let Some((s, t)) = split_qualified(&ret) {
            if cache.tables.contains_key(&(s.clone(), t.clone())) {
                (RetKind::Composite(s, t), Vec::new())
            } else {
                (RetKind::Scalar(ret.clone()), Vec::new())
            }
        } else {
            (RetKind::Scalar(ret.clone()), Vec::new())
        };

        let key = (schema.clone(), name.clone());
        cache.functions.entry(key).or_default().push(FnInfo {
            schema,
            name,
            arg_names,
            required,
            ret_kind,
            out_cols,
            volatile: (provolatile as u8 as char) == 'v',
            is_procedure: (prokind as u8 as char) == 'p',
        });
    }

    // computed relationships
    let rows = client
        .query(&COMPUTED_SQL.replace("{schemas}", &list), &[])
        .await
        .map_err(ApiError::from_db)?;
    for r in rows {
        let fn_schema: String = r.get(0);
        let fn_name: String = r.get(1);
        let arg_schema: String = r.get(2);
        let arg_table: String = r.get(3);
        let ret_schema: String = r.get(4);
        let ret_table: String = r.get(5);
        let setof: bool = r.get(6);
        cache
            .computed
            .entry((arg_schema, arg_table))
            .or_default()
            .push(ComputedRel { fn_schema, fn_name, ret_schema, ret_table, setof });
    }

    // enums
    let rows = client
        .query(&ENUM_SQL.replace("{schemas}", &list), &[])
        .await
        .map_err(ApiError::from_db)?;
    for r in rows {
        let schema: String = r.get(0);
        let name: String = r.get(1);
        let labels: Vec<String> = r.get(2);
        cache.enums.insert((schema, name), labels);
    }

    Ok(cache)
}

/// Count top-level argument separators and DEFAULT clauses in a full
/// function signature, ignoring commas/keywords inside string literals.
fn scan_args_and_defaults(sig: &str) -> (usize, usize) {
    let mut commas = 0usize;
    let mut defaults = 0usize;
    let mut in_q = false;
    let mut depth = 0usize;
    let bytes = sig.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_q {
            if c == '\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_q = false;
                }
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => in_q = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => commas += 1,
            'D' | 'd' if depth == 0 && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) => {
                if sig[i..].len() >= 7 && sig[i..i + 7].eq_ignore_ascii_case("DEFAULT") {
                    let after = bytes.get(i + 7);
                    if after.map(|b| !b.is_ascii_alphanumeric()).unwrap_or(true) {
                        defaults += 1;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    let total = if sig.trim().is_empty() { 0 } else { commas + 1 };
    (total, defaults)
}

fn split_qualified(s: &str) -> Option<(String, String)> {
    let s = s.strip_prefix("SETOF ")?.trim();
    let (a, b) = s.split_once('.')?;
    Some((a.trim_matches('"').to_string(), b.trim_matches('"').to_string()))
}
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}
