// SQL text helpers: identifier/literal quoting, CSV encode/decode.

/// Quote an identifier: `foo"bar` -> `"foo""bar"`.
pub fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Quote a literal string: `o'brien` -> `'o''brien'`.
pub fn ql(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Render a serde_json value as a SQL literal appropriate for an unknown-type
/// parameter (PostgREST semantics: text literals coerce to the column type).
pub fn json_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => ql(s),
        other => ql(&other.to_string()), // array / object as JSON text
    }
}

/// Render a JSON string value as a plain text literal (no JSON quoting).
pub fn text_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => ql(s),
        other => ql(&other.to_string()),
    }
}

// ---------- CSV ----------

pub fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

pub fn csv_write_row(fields: &[String]) -> String {
    fields.iter().map(|f| csv_escape(f)).collect::<Vec<_>>().join(",")
}

/// Minimal RFC4180 state-machine parser. Returns rows of raw fields.
pub fn csv_parse(input: &str) -> Result<Vec<Vec<String>>, String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    row.push(std::mem::take(&mut cur));
                }
                '\r' => { /* tolerate CRLF */ }
                '\n' => {
                    row.push(std::mem::take(&mut cur));
                    rows.push(std::mem::take(&mut row));
                }
                _ => cur.push(c),
            }
        }
    }
    if in_quotes {
        return Err("unterminated quoted field in CSV body".into());
    }
    // trailing row without newline
    if !cur.is_empty() || !row.is_empty() {
        row.push(cur);
        rows.push(row);
    }
    // drop trailing fully-empty rows (e.g. final newline)
    while rows.last().map(|r| r.len() == 1 && r[0].is_empty()).unwrap_or(false) {
        rows.pop();
    }
    Ok(rows)
}
