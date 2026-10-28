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
            if rows.len() > 1 {
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
            let mut cols: Vec<String> = Vec::new();
            if let Some(first) = parsed.first() {
                if let Some(obj) = first.as_object() {
                    for k in obj.keys() {
                        cols.push(k.clone());
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
    // scalar text RPC: single column
    let idx = cols.iter().position(|c| c == "row").unwrap_or(0);
    let v = rows
        .first()
        .and_then(|r| r.get(idx))
        .cloned()
        .flatten()
        .unwrap_or_default();
    Shaped { content_type: "text/plain; charset=utf-8", body: v }
}
