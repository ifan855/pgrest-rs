// Response shaping: arrays, singular objects, CSV; header computation.
use crate::error::ApiError;
use crate::headers::OutMedia;
use crate::sql::csv_write_row;

pub struct Shaped {
    pub content_type: &'static str,
    pub body: String,
}

/// rows are JSON strings (one per row, from to_jsonb output).
pub fn shape(rows: &[String], media: OutMedia) -> Result<Shaped, ApiError> {
    match media {
        OutMedia::Array | OutMedia::Openapi => {
            let mut body = String::with_capacity(16 + rows.iter().map(|r| r.len() + 1).sum::<usize>());
            body.push('[');
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(r);
            }
            body.push(']');
            Ok(Shaped { content_type: "application/json; charset=utf-8", body })
        }
        OutMedia::Object => {
            // PostgREST: singular object requires exactly one row, else 406 PGRST116
            if rows.len() > 1 || rows.is_empty() {
                return Err(ApiError::multiple_objects());
            }
            let body = rows.first().cloned().unwrap_or_else(|| "null".to_string());
            Ok(Shaped { content_type: "application/json; charset=utf-8", body })
        }
        OutMedia::Csv => {
            if rows.is_empty() {
                return Ok(Shaped { content_type: "text/csv; charset=utf-8", body: String::new() });
            }
            let mut parsed: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
            for r in rows {
                parsed.push(serde_json::from_str(r).map_err(|e| ApiError::internal(e.to_string()))?);
            }
            // union of keys across rows, first-seen order (rows may differ)
            let mut cols: Vec<String> = Vec::new();
            for obj in &parsed {
                if let Some(m) = obj.as_object() {
                    for k in m.keys() {
                        if !cols.contains(k) {
                            cols.push(k.clone());
                        }
                    }
                }
            }
            let mut out = String::new();
            out.push_str(&csv_write_row(&cols));
            out.push('\n');
            for obj in &parsed {
                let mut fields = Vec::new();
                for c in &cols {
                    let cell = match obj.get(c) {
                        None | Some(serde_json::Value::Null) => String::new(),
                        Some(serde_json::Value::String(s)) => s.clone(),
                        Some(serde_json::Value::Bool(b)) => b.to_string(),
                        Some(serde_json::Value::Number(n)) => n.to_string(),
                        Some(other) => other.to_string(),
                    };
                    fields.push(cell);
                }
                out.push_str(&csv_write_row(&fields));
                out.push('\n');
            }
            Ok(Shaped { content_type: "text/csv; charset=utf-8", body: out })
        }
    }
}

pub fn shape_scalar_text(rows: &[Vec<Option<String>>], cols: &[String]) -> Shaped {
    // scalar text RPC: single "row" column
    let idx = cols.iter().position(|c| c == "row").unwrap_or(0);
    let vals: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get(idx).cloned().flatten())
        .collect();
    if vals.len() > 1 {
        // bulk scalar calls come back as a JSON array of strings
        let body = vals
            .iter()
            .map(|v| serde_json::json!(v).to_string())
            .collect::<Vec<_>>()
            .join(",");
        Shaped { content_type: "application/json; charset=utf-8", body: format!("[{}]", body) }
    } else {
        Shaped { content_type: "text/plain; charset=utf-8", body: vals.into_iter().next().unwrap_or_default() }
    }
}
