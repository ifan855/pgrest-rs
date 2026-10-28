// URL grammar parser: select tree, filters (incl. logic combinators), order,
// limit/offset. Mirrors PostgREST's grammar as closely as practical.
use crate::ast::*;
use crate::error::ApiError;
use crate::headers::RangeHdr;

fn err<T>(m: impl Into<String>) -> Result<T, ApiError> {
    Err(ApiError::parse(m))
}

/// Split on top-level commas, respecting (...) groups and quoted strings.
pub fn split_top_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_q = false;
    for c in s.chars() {
        if in_q {
            cur.push(c);
            if c == '\'' {
                in_q = false;
            }
            continue;
        }
        match c {
            '\'' => {
                in_q = true;
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    parts
}

/// Parse the `select=` value into a tree of selections/embeds.
pub fn parse_select(src: &str) -> Result<Vec<Sel>, ApiError> {
    let mut out = Vec::new();
    for raw in split_top_commas(src) {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        out.push(parse_sel_item(item)?);
    }
    Ok(out)
}

fn parse_sel_item(item: &str) -> Result<Sel, ApiError> {
    // embed: name(hint-ish args) — find first top-level '('
    let bytes = item.as_bytes();
    let mut open = None;
    let mut depth = 0usize;
    let mut in_q = false;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
        if in_q {
            if c == '\'' {
                in_q = false;
            }
            continue;
        }
        match c {
            '\'' => in_q = true,
            '(' => {
                if depth == 0 {
                    open = Some(i);
                }
                depth += 1;
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if depth != 0 {
        return err(format!("unbalanced parentheses in select item '{}'", item));
    }

    if let Some(open) = open {
        // must end with ')'
        if !item.ends_with(')') {
            return err(format!("malformed select item '{}'", item));
        }
        let head = &item[..open];
        let inner = &item[open + 1..item.len() - 1];
        // aggregates take priority over embeds: count(), sum(col)...
        let fname = head.trim().to_lowercase();
        if matches!(fname.as_str(), "count" | "sum" | "avg" | "min" | "max") {
            let arg = inner.trim();
            return Ok(Sel {
                name: fname.clone(),
                alias: None,
                cast: None,
                json: None,
                agg: Some(Agg {
                    func: fname,
                    arg: if arg.is_empty() { None } else { Some(arg.to_string()) },
                }),
                embed: None,
            });
        }
        return parse_embed(head, inner);
    }

    // plain column with optional alias / cast / json arrow
    let mut sel = Sel {
        name: String::new(),
        alias: None,
        cast: None,
        json: None,
        agg: None,
        embed: None,
    };
    let mut rest = item;

    // alias:col (single colon only; '::' is a cast, not an alias separator)
    if let Some((a, b)) = split_single_colon(rest) {
        if !a.is_empty() && is_ident(a) {
            sel.alias = Some(a.to_string());
            rest = b;
        }
    }

    // cast: name::type
    if let Some(pos) = rest.find("::") {
        let (n, c) = rest.split_at(pos);
        if !c[2..].is_empty() && is_ident(n) {
            sel.name = n.to_string();
            sel.cast = Some(c[2..].to_string());
            return Ok(sel);
        }
    }

    // json arrow
    for (arrow, is_text) in [("->>", true), ("->", false)] {
        if let Some(pos) = rest.find(arrow) {
            let (n, k) = rest.split_at(pos);
            let key = k[arrow.len()..].trim().trim_matches('\'').to_string();
            if !n.is_empty() {
                sel.name = n.to_string();
                sel.json = Some((key, is_text));
                return Ok(sel);
            }
        }
    }

    // aggregates: count() / sum(col)
    if let Some(open) = rest.find('(') {
        let fname = rest[..open].trim().to_lowercase();
        if matches!(fname.as_str(), "count" | "sum" | "avg" | "min" | "max") && rest.ends_with(')') {
            let arg = rest[open + 1..rest.len() - 1].trim();
            sel.agg = Some(Agg {
                func: fname.clone(),
                arg: if arg.is_empty() { None } else { Some(arg.to_string()) },
            });
            sel.name = fname;
            return Ok(sel);
        }
    }

    if rest != "*" && !is_ident(rest) {
        return err(format!("invalid select item '{}'", item));
    }
    sel.name = rest.to_string();
    Ok(sel)
}

fn parse_embed(head: &str, inner: &str) -> Result<Sel, ApiError> {
    // head: rel | rel!hint | rel!inner | rel!hint!inner | alias:rel!hint...
    let mut alias = None;
    let mut head = head;
    if let Some((a, b)) = split_first_unquoted(head, ':') {
        if !a.is_empty() && is_ident(a) {
            alias = Some(a.to_string());
            head = b;
        }
    }
    let mut parts = head.split('!');
    let rel = parts.next().unwrap_or("").trim().to_string();
    if !is_ident(&rel) {
        return err(format!("invalid embed relation '{}'", rel));
    }
    let mut hint = None;
    let mut inner_join = false;
    for p in parts {
        match p.trim() {
            "inner" => inner_join = true,
            other => hint = Some(other.to_string()),
        }
    }
    let mut sub_select = parse_select(inner)?;
    // bare `count` inside an embed means count(*) (PostgREST v12 syntax)
    for s in sub_select.iter_mut() {
        if s.name == "count" && s.agg.is_none() && s.embed.is_none() && s.cast.is_none() && s.json.is_none() {
            s.agg = Some(Agg { func: "count".into(), arg: None });
        }
    }
    Ok(Sel {
        name: rel.clone(),
        alias: alias.or_else(|| Some(rel.clone())),
        cast: None,
        json: None,
        agg: None,
        embed: Some(Box::new(EmbedNode {
            rel,
            hint,
            inner: inner_join,
            select: sub_select,
            filters: Vec::new(),
            order: Vec::new(),
            limit: None,
            offset: None,
        })),
    })
}

/// Find first unquoted occurrence of `c` and split.
fn split_first_unquoted(s: &str, c: char) -> Option<(&str, &str)> {
    let mut in_q = false;
    for (i, ch) in s.char_indices() {
        if in_q {
            if ch == '\'' {
                in_q = false;
            }
            continue;
        }
        match ch {
            '\'' => in_q = true,
            x if x == c => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Split at the first '.' at paren-depth 0 (outside quotes).
fn split_first_top_dot(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut in_q = false;
    for (i, ch) in s.char_indices() {
        if in_q {
            if ch == '\'' {
                in_q = false;
            }
            continue;
        }
        match ch {
            '\'' => in_q = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Split at the first SINGLE ':' (not part of '::').
fn split_single_colon(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b':' {
            if i + 1 < b.len() && b[i + 1] == b':' {
                continue; // '::' cast
            }
            if i > 0 && b[i - 1] == b':' {
                continue; // second half of '::'
            }
            return Some((&s[..i], &s[i + 1..]));
        }
    }
    None
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false)
        && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

// ---------- filters ----------

/// Reserved params that are never filters.
const RESERVED: &[&str] = &[
    "select", "order", "limit", "offset", "columns", "on_conflict", "logic", "filter", "format",
];

pub fn parse_filters(params: &[(String, String)]) -> Result<Vec<Cond>, ApiError> {
    let mut out = Vec::new();
    for (k, v) in params {
        if RESERVED.contains(&k.as_str()) {
            continue;
        }
        out.push(parse_cond(k, v)?);
    }
    Ok(out)
}

fn parse_cond(key: &str, val: &str) -> Result<Cond, ApiError> {
    // two surface forms reach this function:
    //  a) top-level params:      key="year"      val="gte.2014"        (col=op.value)
    //  b) logic blocks / inner:  key="year.gt"   val="2000"            (col.op.value)
    let mut not = false;
    let mut k = key;
    if let Some(rest) = k.strip_prefix("not.") {
        not = true;
        k = rest;
    }
    match k {
        "and" | "or" => {
            let op = if k == "and" { LogicOp::And } else { LogicOp::Or };
            let items = parse_group(val)?;
            Ok(Cond::Group { op, not, items })
        }
        _ => {
            if let Some(dot) = find_op_dot(k) {
                // form b: col.op in the key
                let col = k[..dot].to_string();
                let (op, val2, not2) = split_op_not(&k[dot + 1..], val);
                validate_op(&op)?;
                Ok(Cond::Leaf { col, op, val: val2, not: not || not2 })
            } else {
                // form a: op.value in the query value (op may carry parens: fts(english))
                let (op, v) = split_op_value(val).ok_or_else(|| {
                    ApiError::parse(format!("invalid filter '{}={}', missing operator", key, val))
                })?;
                let (op, v, not2) = split_op_not(&op, &v);
                validate_op(&op)?;
                Ok(Cond::Leaf { col: k.to_string(), op, val: v, not: not || not2 })
            }
        }
    }
}

/// Handle `not.` prefix on the operator segment; returns (op, value, negated).
fn split_op_not(op: &str, val: &str) -> (String, String, bool) {
    if let Some(base) = op.strip_prefix("not.") {
        (base.to_string(), val.to_string(), true)
    } else if op == "not" {
        // form: col=not.eq.value (the dot was consumed by the caller)
        (val.to_string(), String::new(), true)
    } else {
        (op.to_string(), val.to_string(), false)
    }
}

/// Split "op.value" where op may contain parens: fts(english).value, or a
/// negation: not.eq.value. Returns (op, value).
fn split_op_value(v: &str) -> Option<(String, String)> {
    // negation form: not.<op>.<value>
    if let Some(rest) = v.strip_prefix("not.") {
        let (op, val) = split_op_value(rest)?;
        return Some((format!("not.{}", op), val));
    }
    // op(lang).value
    if let Some(open) = v.find('(') {
        if let Some(close) = v[open..].find(')') {
            let close = open + close;
            if v.len() > close + 1 && v.as_bytes()[close + 1] == b'.' {
                return Some((v[..=close].to_string(), v[close + 2..].to_string()));
            }
        }
    }
    let (a, b) = v.split_once('.')?;
    Some((a.to_string(), b.to_string()))
}

/// Operator dot position: first '.' outside quotes and parens, not part of '::'.
fn find_op_dot(s: &str) -> Option<usize> {
    let mut in_q = false;
    let mut depth = 0usize;
    let b = s.as_bytes();
    for i in 0..b.len() {
        let c = b[i] as char;
        if in_q {
            if c == '\'' {
                in_q = false;
            }
            continue;
        }
        match c {
            '\'' => in_q = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => {
                if i + 1 < b.len() && b[i + 1] == b'.' {
                    return None; // '::' cast, not an operator boundary
                }
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

fn validate_op(op: &str) -> Result<(), ApiError> {
    const OPS: &[&str] = &[
        "eq", "neq", "gt", "gte", "lt", "lte", "like", "ilike", "match", "imatch", "in", "is",
        "isdistinct", "cs", "cd", "ov", "sl", "sr", "nxl", "nxr", "adj", "fts", "plfts", "phfts",
        "websearch",
    ];
    let base = op.split('(').next().unwrap_or(op).split('.').next().unwrap_or(op);
    if OPS.contains(&base) {
        Ok(())
    } else {
        err(format!("unknown filter operator '{}'", op))
    }
}

fn parse_group(v: &str) -> Result<Vec<Cond>, ApiError> {
    let v = v.trim();
    if !(v.starts_with('(') && v.ends_with(')')) {
        return err(format!("logic block must be parenthesized, got '{}'", v));
    }
    let inner = &v[1..v.len() - 1];
    let mut items = Vec::new();
    for part in split_top_commas(inner) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        // nested logic blocks: and=(...), or=(...), not.and=(...)
        if let Some((k, v)) = p.split_once('=') {
            if k == "and" || k == "or" || k.starts_with("not.") {
                items.push(parse_cond(k, v)?);
                continue;
            }
        }
        // col.op.value
        let (col, rv) = split_first_top_dot(p).ok_or_else(|| {
            ApiError::parse(format!("invalid condition '{}' in logic block", p))
        })?;
        let (op, val) = split_first_top_dot(rv).ok_or_else(|| {
            ApiError::parse(format!("invalid condition '{}' in logic block", p))
        })?;
        items.push(parse_cond(&format!("{}.{}", col, op), val)?)
    }
    Ok(items)
}

/// Route embed-scoped params like `actors.name=eq.X`, `actors.order=...`,
/// `actors.limit=...` into the embed nodes of the select tree.
pub fn route_embed_params(
    select: &mut Vec<Sel>,
    params: &[(String, String)],
) -> Result<Vec<Cond>, ApiError> {
    let mut root_conds = Vec::new();
    let embed_names: Vec<String> = select
        .iter()
        .filter_map(|s| s.embed.as_ref().map(|e| e.rel.clone()))
        .collect();
    for (k, v) in params {
        if RESERVED.contains(&k.as_str()) {
            continue;
        }
        if let Some((rel, rest)) = k.split_once('.') {
            if embed_names.contains(&rel.to_string()) {
                if let Some(sel) = select.iter_mut().find(|s| {
                    s.embed.as_ref().map(|e| e.rel == rel).unwrap_or(false)
                }) {
                    let node = sel.embed.as_mut().unwrap();
                    match rest {
                        "order" => node.order = parse_order(v)?,
                        "limit" => node.limit = Some(v.parse().map_err(|_| ApiError::parse(format!("invalid limit '{}'", v)))?),
                        "offset" => node.offset = Some(v.parse().map_err(|_| ApiError::parse(format!("invalid offset '{}'", v)))?),
                        _ => node.filters.push(parse_cond(rest, v)?),
                    }
                    continue;
                }
            }
        }
        if k == "and" || k == "or" || k.starts_with("not.") {
            root_conds.push(parse_cond(k, v)?);
        } else if k.contains('.') {
            // maybe filter with operator: col.op.value where col itself has dots? handled by parse_cond
            root_conds.push(parse_cond(k, v)?);
        } else {
            root_conds.push(parse_cond(k, v)?);
        }
    }
    Ok(root_conds)
}

// ---------- order ----------

pub fn parse_order(v: &str) -> Result<Vec<OrderItem>, ApiError> {
    let mut out = Vec::new();
    for part in split_top_commas(v) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let mut it = OrderItem { col: String::new(), dir: None, nulls: None };
        // segments after col are dir/nulls keywords
        let mut segs: Vec<String> = p.split('.').map(|s| s.to_string()).collect();
        it.col = segs.remove(0);
        for seg in segs {
            match seg.to_lowercase().as_str() {
                "asc" => it.dir = Some(Dir::Asc),
                "desc" => it.dir = Some(Dir::Desc),
                "nullsfirst" => it.nulls = Some(Nulls::First),
                "nullslast" => it.nulls = Some(Nulls::Last),
                other => {
                    // allow json arrow continuation like data->>k
                    it.col.push('.');
                    it.col.push_str(other);
                }
            }
        }
        out.push(it);
    }
    Ok(out)
}

// ---------- range / limit ----------

pub fn apply_range(q: &mut Query, r: Option<RangeHdr>) {
    if let Some(r) = r {
        let last = r.last.unwrap_or(i64::MAX - 1);
        q.offset = Some(r.first);
        q.limit = Some(last.saturating_sub(r.first).saturating_add(1));
        q.range = Some(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_select_with_alias_cast_json() {
        let sels = parse_select("title,rating::text,aliased:year,data->>nested,data->obj").unwrap();
        assert_eq!(sels[0].name, "title");
        assert_eq!(sels[1].cast.as_deref(), Some("text"));
        assert_eq!(sels[2].alias.as_deref(), Some("aliased"));
        assert_eq!(sels[3].json.as_ref().unwrap().0, "nested");
        assert!(sels[3].json.as_ref().unwrap().1); // text arrow
        assert!(!sels[4].json.as_ref().unwrap().1);
    }

    #[test]
    fn parses_embed_tree_with_hint_and_inner() {
        let sels = parse_select("title,actors!inner(name),director:directors!director_id(name,films(*))").unwrap();
        let e = sels[1].embed.as_ref().unwrap();
        assert_eq!(e.rel, "actors");
        assert!(e.inner);
        let e2 = sels[2].embed.as_ref().unwrap();
        assert_eq!(e2.rel, "directors");
        assert_eq!(sels[2].alias.as_deref(), Some("director"));
        let nested = e2.select[1].embed.as_ref().unwrap();
        assert_eq!(nested.rel, "films");
    }

    #[test]
    fn parses_top_level_aggregates() {
        let sels = parse_select("count(),sum(rating),total:count(year)").unwrap();
        assert!(sels[0].agg.is_some());
        assert_eq!(sels[0].agg.as_ref().unwrap().func, "count");
        assert_eq!(sels[1].agg.as_ref().unwrap().arg.as_deref(), Some("rating"));
        assert_eq!(sels[2].alias.as_deref(), Some("total"));
    }

    #[test]
    fn parses_embed_count_aggregate() {
        let sels = parse_select("title,reviews(count)").unwrap();
        let e = sels[1].embed.as_ref().unwrap();
        assert_eq!(e.select[0].agg.as_ref().unwrap().func, "count");
    }

#[test]
fn dbg_params() {
    let params: Vec<(String,String)> = vec![
        ("title".into(), "eq.Dune".into()),
        ("year".into(), "gte.2000".into()),
        ("title".into(), "not.eq.X".into()),
        ("or".into(), "(and=(a.eq.1,b.eq.2),c.eq.3)".into()),
        ("tags".into(), "cs.{scifi,thriller}".into()),
        ("search".into(), "fts(english).dream".into()),
        ("search".into(), "fts.heist".into()),
        ("title".into(), "in.(Dune,Barbie)".into()),
        ("year".into(), "is.null".into()),
    ];
    for (k, v) in &params {
        match parse_cond(k, v) {
            Ok(_) => println!("OK   {}={}", k, v),
            Err(e) => println!("ERR  {}={} -> {}", k, v, e.message),
        }
    }
}

    #[test]
    fn parses_filters_all_forms() {
        let params = vec![
            ("title".to_string(), "eq.Dune".to_string()),
            ("year".to_string(), "gte.2000".to_string()),
            ("title".to_string(), "not.eq.X".to_string()),
            ("and".to_string(), "(a.eq.1,b.lt.5)".to_string()),
            ("or".to_string(), "(and=(a.eq.1,b.eq.2),c.eq.3)".to_string()),
            ("tags".to_string(), "cs.{scifi,thriller}".to_string()),
            ("search".to_string(), "fts(english).dream".to_string()),
            ("search".to_string(), "fts.heist".to_string()),
            ("title".to_string(), "in.(Dune,Barbie)".to_string()),
            ("year".to_string(), "is.null".to_string()),
        ];
        let conds = parse_filters(&params).unwrap();
        assert_eq!(conds.len(), 10);
        if let Cond::Leaf { op, val, not, .. } = &conds[2] {
            assert_eq!(op, "eq");
            assert_eq!(val, "X");
            assert!(*not);
        } else {
            panic!("expected leaf");
        }
        if let Cond::Group { op, items, .. } = &conds[4] {
            assert!(matches!(op, LogicOp::Or));
            assert!(matches!(&items[0], Cond::Group { .. }));
        } else {
            panic!("expected group");
        }
    }

    #[test]
    fn parses_order_multi() {
        let o = parse_order("year.desc.nullslast,title,data->>k.asc").unwrap();
        assert_eq!(o[0].dir, Some(Dir::Desc));
        assert_eq!(o[0].nulls, Some(Nulls::Last));
        assert_eq!(o[1].dir, None);
        assert!(o[2].col.contains("data"));
    }

    #[test]
    fn rejects_bad_operators() {
        let params = vec![("title".to_string(), "bogus.1".to_string())];
        assert!(parse_filters(&params).is_err());
    }

    #[test]
    fn csv_roundtrip() {
        use crate::sql::csv_parse;
        let rows = csv_parse("a,b\n1,\"x,\"\"y\"\"\n\"\n2,z\n").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][1], "x,\"y\"\n");
    }
}
