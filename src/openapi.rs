// OpenAPI 2.0 generation from the schema cache.
use crate::schema::{RetKind, SchemaCache, TableInfo};
use serde_json::{json, Value};

/// Operator set documented per column type family (PostgREST semantics).
fn filter_ops_for(coltype: &str) -> &'static str {
    if coltype.ends_with(']') {
        "eq, neq, cs, cd, ov, sl, sr, nxl, nxr, adj, is"
    } else if coltype.starts_with("json") || coltype.starts_with("array") {
        "eq, neq, gt, gte, lt, lte, like, ilike, in, is, cs, cd, ov"
    } else if coltype == "tsvector" || coltype.contains("text") || coltype.contains("character") || coltype.contains("uuid") || coltype.contains("date") || coltype.contains("time") {
        "eq, neq, gt, gte, lt, lte, like, ilike, match, imatch, in, is, isdistinct, fts, plfts, phfts, websearch"
    } else if coltype.contains("int") || coltype.contains("numeric") || coltype.contains("real") || coltype.contains("double") || coltype.contains("money") || coltype.contains("oid") {
        "eq, neq, gt, gte, lt, lte, in, is, isdistinct"
    } else if coltype == "boolean" {
        "eq, is"
    } else {
        "eq, neq, gt, gte, lt, lte, like, ilike, in, is, isdistinct"
    }
}

pub fn generate(cache: &SchemaCache) -> Value {
    let mut paths = serde_json::Map::new();
    let mut definitions = serde_json::Map::new();

    let mut tables: Vec<&TableInfo> = cache.tables.values().collect();
    tables.sort_by(|a, b| (a.schema.clone(), a.name.clone()).cmp(&(b.schema.clone(), b.name.clone())));

    for t in &tables {
        let path = format!("/{}/{}", t.schema, t.name);
        let mut op_get = json!({
            "tags": [t.schema.clone()],
            "produces": ["application/json", "text/csv"],
            "parameters": [
                {"name": "select", "in": "query", "type": "string", "description": "Vertical filtering with columns, aggregates and related resources"},
                {"name": "order", "in": "query", "type": "string", "description": "Ordering, e.g. id.desc.nullslast"},
                {"name": "limit", "in": "query", "type": "integer"},
                {"name": "offset", "in": "query", "type": "integer"},
                {"name": "Range", "in": "header", "type": "string", "description": "items=0-14"}
            ],
            "responses": {"200": {"description": "OK"}}
        });
        if let Some(c) = &t.comment {
            op_get["description"] = json!(c);
        }
        // column filters with type-aware operator documentation
        for col in &t.columns {
            let pname = format!("{}.{}", col.name, "*");
            op_get["parameters"].as_array_mut().unwrap().push(json!({
                "name": pname, "in": "query", "type": "string",
                "description": format!(
                    "Filter on {} ({}). Operators: {}",
                    col.name,
                    col.type_name,
                    filter_ops_for(&col.type_name)
                )
            }));
        }
        let op_post = json!({
            "tags": [t.schema.clone()],
            "consumes": ["application/json", "text/csv"],
            "produces": ["application/json"],
            "parameters": [{"name": "body", "in": "body", "schema": {"$ref": format!("#/definitions/{}", t.name)}}],
            "responses": {"201": {"description": "Created"}, "204": {"description": "No Content"}}
        });
        let op_patch = json!({
            "tags": [t.schema.clone()],
            "consumes": ["application/json"],
            "parameters": [{"name": "body", "in": "body", "schema": {"$ref": format!("#/definitions/{}", t.name)}}],
            "responses": {"200": {"description": "OK"}, "204": {"description": "No Content"}}
        });
        let op_delete = json!({
            "tags": [t.schema.clone()],
            "responses": {"200": {"description": "OK"}, "204": {"description": "No Content"}}
        });

        let mut methods = json!({
            "get": op_get,
            "post": op_post,
            "patch": op_patch,
            "delete": op_delete
        });
        // PUT upsert-by-PK on tables with a primary key
        if t.is_table() && !t.pks.is_empty() {
            methods["put"] = json!({
                "tags": [t.schema.clone()],
                "consumes": ["application/json"],
                "description": "Upsert by primary key: update the row, or insert it when missing. Requires the PK columns in the query string and the body.",
                "parameters": [
                    {"name": "body", "in": "body", "required": true, "schema": {"$ref": format!("#/definitions/{}", t.name)}}
                ],
                "responses": {"200": {"description": "OK"}, "204": {"description": "No Content"}}
            });
        }
        paths.insert(path, methods);

        // definition
        let mut props = serde_json::Map::new();
        let mut required: Vec<Value> = Vec::new();
        for col in &t.columns {
            let (ty, fmt) = pg_type_to_swagger(&col.type_name);
            let mut p = json!({"type": ty});
            if let Some(f) = fmt {
                p["format"] = json!(f);
            }
            if let Some(labels) = cache.enums.get(&(t.schema.clone(), col.type_name.clone())) {
                p["enum"] = json!(labels);
            }
            if let Some(c) = &col.comment {
                p["description"] = json!(c);
            }
            props.insert(col.name.clone(), p);
            if !col.nullable && !col.has_default && !col.generated {
                required.push(json!(col.name));
            }
        }
        let mut def = json!({"type": "object", "properties": props});
        if !required.is_empty() {
            def["required"] = json!(required);
        }
        if let Some(c) = &t.comment {
            def["description"] = json!(c);
        }
        definitions.insert(t.name.clone(), def);
    }

    // rpc paths (all overloads flattened)
    let mut fns: Vec<&crate::schema::FnInfo> = cache
        .functions
        .values()
        .flat_map(|v| v.iter())
        .collect();
    fns.sort_by(|a, b| (a.schema.clone(), a.name.clone()).cmp(&(b.schema.clone(), b.name.clone())));
    let mut seen_fn_paths: Vec<String> = Vec::new();
    for f in &fns {
        let path = format!("/rpc/{}", f.name);
        if seen_fn_paths.contains(&path) {
            continue;
        }
        seen_fn_paths.push(path.clone());
        let mut params: Vec<Value> = Vec::new();
        for a in &f.arg_names {
            params.push(json!({"name": a, "in": "query", "type": "string", "required": false}));
        }
        let kind_desc = retkind_desc(&f.ret_kind);
        let methods = if f.is_procedure {
            json!({"post": {
                "tags": [f.schema.clone()],
                "description": format!("Database procedure (CALL); return kind: {}", kind_desc),
                "consumes": ["application/json"],
                "parameters": [{"name": "args", "in": "body", "schema": {"type": "object"}}],
                "responses": {"204": {"description": "No Content"}}
            }})
        } else if f.volatile {
            json!({"post": {
                "tags": [f.schema.clone()],
                "description": format!("Database function; return kind: {}", kind_desc),
                "consumes": ["application/json"],
                "parameters": [{"name": "args", "in": "body", "schema": {"type": "object"}}],
                "responses": {"200": {"description": "OK"}, "204": {"description": "No Content"}}
            }})
        } else {
            json!({
                "get": {"tags": [f.schema.clone()], "parameters": params, "responses": {"200": {"description": "OK"}}},
                "post": {"tags": [f.schema.clone()], "consumes": ["application/json"],
                    "parameters": [{"name": "args", "in": "body", "schema": {"type": "object"}}],
                    "responses": {"200": {"description": "OK"}, "204": {"description": "No Content"}}}
            })
        };
        paths.insert(path, methods);
    }

    json!({
        "swagger": "2.0",
        "info": {
            "title": "pgrest-rs",
            "description": "A PostgREST-compatible REST API for PostgreSQL, auto-generated by pgrest-rs",
            "version": "0.3.0 (pgrest-rs)"
        },
        "host": "",
        "basePath": "/",
        "schemes": ["http"],
        "consumes": ["application/json"],
        "produces": ["application/json"],
        "securityDefinitions": {
            "bearerAuth": {"type": "apiKey", "in": "header", "name": "Authorization", "description": "JWT: Authorization: Bearer <token> (HS256 or RS256 via jwks-url)"}
        },
        "paths": Value::Object(paths),
        "definitions": Value::Object(definitions)
    })
}

fn pg_type_to_swagger(t: &str) -> (&'static str, Option<&'static str>) {
    match t {
        "integer" | "smallint" | "bigint" => ("integer", Some("int64")),
        "numeric" | "real" | "double precision" => ("number", Some("double")),
        "boolean" => ("boolean", None),
        "timestamp with time zone" => ("string", Some("date-time")),
        "timestamp without time zone" => ("string", Some("date-time")),
        "date" => ("string", Some("date")),
        "json" | "jsonb" => ("object", None),
        "text" | "character varying" | "character" | "tsvector" => ("string", None),
        _ => {
            if t.ends_with(']') {
                ("array", None)
            } else {
                ("string", None)
            }
        }
    }
}

fn retkind_desc(r: &RetKind) -> &'static str {
    match r {
        RetKind::Void => "void",
        RetKind::Scalar(_) => "scalar",
        RetKind::Composite(_, _) => "composite",
        RetKind::SetOfScalar(_) => "setof scalar",
        RetKind::SetOf(_, _) => "setof",
        RetKind::Table => "table",
    }
}
