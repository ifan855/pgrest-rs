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
    pub ret_kind: RetKind,
    pub volatile: bool,
    pub is_procedure: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RetKind {
    Void,
    Scalar(String),   // type name
    Composite(String, String), // schema.table
    SetOf(String, String),
    Table, // returns table(...) -- treated as setof composite via OUT cols
}

#[derive(Debug, Default)]
pub struct SchemaCache {
    pub tables: HashMap<(String, String), TableInfo>,
    pub functions: HashMap<(String, String), FnInfo>,
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

    pub fn function(&self, schema: &str, name: &str) -> Option<&FnInfo> {
        self.functions.get(&(schema.to_string(), name.to_string()))
    }

    pub fn find_function(&self, schemas: &[String], name: &str) -> Result<&FnInfo, FindErr> {
        let mut hits: Vec<&FnInfo> = Vec::new();
        for s in schemas {
            if let Some(f) = self.function(s, name) {
                hits.push(f);
            }
        }
        match hits.len() {
            0 => Err(FindErr::Missing),
            1 => Ok(hits[0]),
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
WHERE n.nspname = ANY({schemas}) AND c.relkind IN ('r','v','m','p')
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
       (SELECT coalesce(array_agg(a.attname), '{}')
          FROM unnest(p.proallargtypes) WITH ORDINALITY AS at(t, i)
          JOIN pg_attribute a ON false) IS NULL AS plain,
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
        let ret: String = r.get(5);
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

        let ret_kind = if ret == "void" {
            RetKind::Void
        } else if let Some((s, t)) = split_qualified(&ret) {
            if ret.starts_with("SETOF ") {
                RetKind::SetOf(s, t)
            } else if cache.tables.contains_key(&(s.clone(), t.clone())) {
                RetKind::Composite(s, t)
            } else {
                RetKind::Scalar(ret.clone())
            }
        } else {
            RetKind::Scalar(ret.clone())
        };

        let key = (schema.clone(), name.clone());
        cache.functions.insert(
            key,
            FnInfo {
                schema,
                name,
                arg_names,
                ret_kind,
                volatile: (provolatile as u8 as char) == 'v',
                is_procedure: (prokind as u8 as char) == 'p',
            },
        );
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

fn split_qualified(s: &str) -> Option<(String, String)> {
    let s = s.strip_prefix("SETOF ")?.trim();
    let (a, b) = s.split_once('.')?;
    Some((a.trim_matches('"').to_string(), b.trim_matches('"').to_string()))
}
