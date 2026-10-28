// Request header parsing: Prefer, Range, Accept negotiation.
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct Prefer {
    pub count: Option<String>,       // exact | planned | estimated | none
    pub ret: Option<String>,         // representation | minimal | headers-only
    pub resolution: Option<String>,  // merge-duplicates | ignore-duplicates
    pub missing: Option<String>,     // default | null
    pub params: Option<String>,      // single-object | multiple-objects
    pub tx: Option<String>,          // commit | rollback
    pub plan: bool,                  // planning
    pub unknown: Vec<String>,
}

impl Prefer {
    pub fn parse(h: Option<&str>) -> Prefer {
        let mut p = Prefer::default();
        let Some(h) = h else { return p };
        for part in h.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                Some(("count", v)) => p.count = Some(v.trim().to_string()),
                Some(("return", v)) => p.ret = Some(v.trim().to_string()),
                Some(("resolution", v)) => p.resolution = Some(v.trim().to_string()),
                Some(("missing", v)) => p.missing = Some(v.trim().to_string()),
                Some(("params", v)) => p.params = Some(v.trim().to_string()),
                Some(("tx", v)) => p.tx = Some(v.trim().to_string()),
                Some((k, v)) => p.unknown.push(format!("{}={}", k, v)),
                None => match part {
                    "planning" | "plan" => p.plan = true,
                    other => p.unknown.push(other.to_string()),
                },
            }
        }
        p
    }

    pub fn applied(&self) -> Option<String> {
        let mut v: Vec<String> = Vec::new();
        if let Some(c) = &self.count {
            v.push(format!("count={}", c));
        }
        if let Some(r) = &self.ret {
            v.push(format!("return={}", r));
        }
        if let Some(r) = &self.resolution {
            v.push(format!("resolution={}", r));
        }
        if let Some(m) = &self.missing {
            v.push(format!("missing={}", m));
        }
        if let Some(t) = &self.tx {
            v.push(format!("tx={}", t));
        }
        if self.plan {
            v.push("planning".to_string());
        }
        if v.is_empty() {
            None
        } else {
            Some(v.join(", "))
        }
    }
}

/// Range header for keyset-free pagination: `Range: items=0-14` (inclusive).
#[derive(Debug, Clone)]
pub struct RangeHdr {
    pub first: i64,
    pub last: Option<i64>,
}

pub fn parse_range(h: Option<&str>, unit: Option<&str>) -> Result<Option<RangeHdr>, String> {
    let Some(h) = h else { return Ok(None) };
    let h = h.trim();
    // Two accepted forms: `Range: items=0-14` and, with `Range-Unit: items`,
    // the bare `Range: 0-14`.
    let (unit_name, spec) = match h.split_once('=') {
        Some((u, s)) => (u.trim().to_string(), s.trim().to_string()),
        None => (unit.unwrap_or("items").trim().to_string(), h.to_string()),
    };
    if unit_name != "items" {
        // Unrecognized range unit: ignore it per RFC, PostgREST does the same.
        return Ok(None);
    }
    let spec = spec.trim();
    let (a, b) = spec
        .split_once('-')
        .ok_or_else(|| format!("invalid Range items spec '{}'", spec))?;
    let first: i64 = a
        .trim()
        .parse()
        .map_err(|_| format!("invalid Range first '{}'", a))?;
    let last = if b.trim().is_empty() {
        None
    } else {
        Some(b.trim().parse::<i64>().map_err(|_| format!("invalid Range last '{}'", b))?)
    };
    if first < 0 {
        return Err("Range first must be >= 0".into());
    }
    Ok(Some(RangeHdr { first, last }))
}

/// Negotiated output media for a rows response.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum OutMedia {
    Array,       // application/json (default) or vnd.pgrst.array
    Object,      // vnd.pgrst.object+json
    Csv,         // text/csv
    Openapi,     // application/openapi+json (root)
}

pub fn negotiate_accept(h: Option<&str>) -> Result<OutMedia, String> {
    let Some(h) = h else { return Ok(OutMedia::Array) };
    if h.contains('*') || h.contains("application/json") || h.contains("text/json") {
        // wildcard wins unless a more specific vnd.pgrst type also present
        if h.contains("vnd.pgrst.object") {
            return Ok(OutMedia::Object);
        }
        if h.contains("vnd.pgrst.array") {
            return Ok(OutMedia::Array);
        }
        if h.contains("text/csv") {
            return Ok(OutMedia::Csv);
        }
        return Ok(OutMedia::Array);
    }
    for part in h.split(',') {
        let p = part.trim();
        if p.starts_with("application/vnd.pgrst.object") {
            return Ok(OutMedia::Object);
        }
        if p.starts_with("application/vnd.pgrst.array") {
            return Ok(OutMedia::Array);
        }
        if p.starts_with("text/csv") {
            return Ok(OutMedia::Csv);
        }
        if p.starts_with("application/openapi+json") {
            return Ok(OutMedia::Openapi);
        }
    }
    Err(format!("requested media type not supported: '{}'", h))
}

/// Parse a query string into ordered (name, value) pairs, treating '+' as space
/// and percent-decoding both sides.
pub fn parse_query(q: Option<&str>) -> Vec<(String, String)> {
    let Some(q) = q else { return Vec::new() };
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.push((decode_component(k), decode_component(v)));
    }
    out
}

pub fn decode_component(s: &str) -> String {
    let s = s.replace('+', " ");
    percent_encoding::percent_decode_str(&s)
        .decode_utf8_lossy()
        .to_string()
}

pub fn encode_component(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

pub type Params = HashMap<String, Vec<String>>;

pub fn params_map(pairs: &[(String, String)]) -> Params {
    let mut m: Params = HashMap::new();
    for (k, v) in pairs {
        m.entry(k.clone()).or_default().push(v.clone());
    }
    m
}
