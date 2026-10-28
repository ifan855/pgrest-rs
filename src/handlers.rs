// HTTP entry: routing, method dispatch, response headers. The single wildcard
// route keeps path parsing explicit (PostgREST-style /rpc/fn, /schema/table).
use crate::ast::Query;
use crate::compiler::{self, CountMode};
use crate::config::TxEnd;
use crate::error::ApiError;
use crate::headers::{self, OutMedia, Prefer};
use crate::media;
use crate::mutate::{self, Resolution};
use crate::pg::{self, ExecCtx};
use crate::schema::{FindErr, SchemaCache, TableInfo};
use crate::sql;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use std::time::Instant;

fn cors(mut resp: Response) -> Response {
    let hs = resp.headers_mut();
    hs.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    hs.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static(
            "Authorization, Content-Type, Content-Profile, Accept, Accept-Profile, Prefer, Range, Range-Unit",
        ),
    );
    hs.insert(
        "Access-Control-Expose-Headers",
        HeaderValue::from_static(
            "Content-Range, Content-Location, Preference-Applied, PostgREST-Plan, Server-Timing",
        ),
    );
    hs.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, PATCH, PUT, DELETE, OPTIONS"),
    );
    resp
}

fn mk_response(status: u16, hdrs: Vec<(&'static str, String)>, body: String) -> Response {
    let mut b = axum::http::Response::builder()
        .status(axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::OK));
    for (k, v) in hdrs {
        b = b.header(k, v);
    }
    b.body(Body::from(body)).unwrap_or_else(|e| ApiError::internal(e.to_string()).into_response())
}

fn err_resp(e: ApiError) -> Response {
    cors(e.into_response())
}

pub async fn entry(State(app): State<Arc<crate::App>>, req: Request) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|s| s.to_string());
    let segments: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(headers::decode_component)
        .collect();

    let resp = if method == Method::OPTIONS {
        mk_response(200, vec![("Content-Length", "0".to_string())], String::new())
    } else {
        dispatch(app.clone(), req, method.clone(), segments, query.clone()).await
    };

    let dur_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut resp = resp;
    let status = resp.status().as_u16();
    app.metrics.record(status, dur_ms);
    // Server-Timing on API responses
    let st = format!("pgrst;dur={:.3}", dur_ms);
    if !resp.headers().contains_key("Server-Timing") {
        if let Ok(v) = HeaderValue::from_str(&st) {
            resp.headers_mut().insert("Server-Timing", v);
        }
    }
    cors(resp)
}

async fn dispatch(
    app: Arc<crate::App>,
    req: Request,
    method: Method,
    segments: Vec<String>,
    query: Option<String>,
) -> Response {
    match segments.first().map(|s| s.as_str()) {
        None => root_get(&app).await,
        Some("admin") => admin(&app, &segments, &method).await,
        Some("rpc") => {
            if segments.len() < 2 {
                err_resp(ApiError::parse("missing function name after /rpc/"))
            } else {
                rpc_handler(app, req, method, segments[1].clone(), query).await
            }
        }
        _ => table_handler(app, req, method, segments, query).await,
    }
}

async fn root_get(app: &Arc<crate::App>) -> Response {
    let cache = app.cache.read().await.clone();
    let doc = crate::openapi::generate(&cache);
    mk_response(
        200,
        vec![("Content-Type", "application/openapi+json; charset=utf-8".to_string())],
        doc.to_string(),
    )
}

async fn admin(app: &Arc<crate::App>, segments: &[String], method: &Method) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return err_resp(ApiError::new(405, "PGRST102", "method not allowed on admin endpoints")
            .with_details("allowed: GET, HEAD"));
    }
    match segments.get(1).map(|s| s.as_str()) {
        Some("live") => mk_response(200, vec![], "OK".into()),
        Some("ready") => {
            let cache = app.cache.read().await;
            if cache.tables.is_empty() {
                mk_response(503, vec![], "NOT READY".into())
            } else {
                mk_response(200, vec![], "OK".into())
            }
        }
        Some("metrics") => {
            let pool_status = app.pool.status();
            let body = app.metrics.render(
                pool_status.available as u64,
                (pool_status.available + pool_status.waiting + pool_status.size) as u64,
            );
            mk_response(
                200,
                vec![("Content-Type", "text/plain; version=0.0.4".to_string())],
                body,
            )
        }
        _ => err_resp(ApiError::new(404, "PGRST118", "unknown admin endpoint")),
    }
}

struct ReqCtx {
    cfg: crate::config::Config,
    cache: Arc<SchemaCache>,
    prefer: Prefer,
    params: Vec<(String, String)>,
    role: String,
    claims: Option<serde_json::Value>,
    accept_raw: Option<String>,
    /// effective schemas (narrowed by Content-Profile / Accept-Profile)
    schemas: Vec<String>,
    /// the profile header value when one narrowed the schema set
    profile: Option<String>,
    /// original raw query string (for Content-Location echo)
    raw_query: Option<String>,
}

struct ReqInfo {
    prefer: Prefer,
    auth: Option<String>,
    query: Option<String>,
    range: Option<headers::RangeHdr>,
    accept: Option<String>,
    content_profile: Option<String>,
    accept_profile: Option<String>,
}

fn req_info(req: &Request) -> ReqInfo {
    ReqInfo {
        prefer: Prefer::parse(req.headers().get("Prefer").and_then(|v| v.to_str().ok())),
        auth: req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
        query: req.uri().query().map(|s| s.to_string()),
        range: headers::parse_range(
            req.headers().get("Range").and_then(|v| v.to_str().ok()),
            req.headers().get("Range-Unit").and_then(|v| v.to_str().ok()),
        )
        .ok()
        .flatten(),
        accept: req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).map(|s| s.to_string()),
        content_profile: req.headers().get("Content-Profile").and_then(|v| v.to_str().ok()).map(|s| s.to_string()),
        accept_profile: req.headers().get("Accept-Profile").and_then(|v| v.to_str().ok()).map(|s| s.to_string()),
    }
}

async fn prepare(app: &Arc<crate::App>, ri: ReqInfo, method: &Method) -> Result<ReqCtx, ApiError> {
    let cfg = app.cfg.read().await.clone();
    let cache = app.cache.read().await.clone();
    let prefer = ri.prefer;
    let params = headers::parse_query(ri.query.as_deref());

    // auth
    let mut role = cfg.db_anon_role.clone();
    let mut claims = None;
    let keys_configured = cfg.jwt_secret.is_some() || cfg.jwks_url.is_some();
    if let Some(auth) = ri.auth {
        if let Some(token) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            if !keys_configured {
                // no secret and no jwks-url: tokens are ignored (documented);
                // requests run as the anonymous role
            } else {
                match crate::auth::verify_token(token, &cfg, &app.jwks).await {
                    Ok(c) => {
                        role = crate::auth::extract_role(&c, &cfg.db_anon_role);
                        claims = Some(c);
                    }
                    Err(e) => return Err(ApiError::unauthorized().with_details(e)),
                }
            }
        } else if !auth.is_empty() {
            return Err(ApiError::unauthorized().with_details("unsupported authorization scheme"));
        }
    }

    // Content-Profile (writes) / Accept-Profile (reads) narrow the schema set
    let is_read = *method == Method::GET || *method == Method::HEAD;
    let profile = if is_read {
        ri.accept_profile.clone().or_else(|| ri.content_profile.clone())
    } else {
        ri.content_profile.clone().or_else(|| ri.accept_profile.clone())
    };
    let schemas = match &profile {
        Some(p) => {
            if !cfg.db_schemas.contains(p) {
                return Err(ApiError::not_found_relation("schema", p));
            }
            vec![p.clone()]
        }
        None => cfg.db_schemas.clone(),
    };

    Ok(ReqCtx { cfg, cache, prefer, params, role, claims, accept_raw: ri.accept, schemas, profile, raw_query: ri.query })
}

fn resolve_table<'c>(
    cache: &'c SchemaCache,
    schemas: &[String],
    segments: &[String],
    profile: Option<&str>,
) -> Result<&'c TableInfo, ApiError> {
    let name = segments.last().unwrap();
    match segments.len() {
        1 => match cache.find_table(schemas, name) {
            Ok(t) => Ok(t),
            Err(FindErr::Missing) => Err(ApiError::not_found_relation("table", name)),
            Err(FindErr::Ambiguous) => Err(ApiError::ambiguous("table", name)),
        },
        2 => {
            // with a profile header the profile schema wins over the path segment
            let schema = profile.unwrap_or(&segments[0]).to_string();
            cache
                .table(&schema, &segments[1])
                .ok_or_else(|| ApiError::not_found_relation("table", &format!("{}.{}", schema, segments[1])))
        }
        _ => Err(ApiError::parse("unexpected path segments")),
    }
}

async fn table_handler(
    app: Arc<crate::App>,
    req: Request,
    method: Method,
    segments: Vec<String>,
    query: Option<String>,
) -> Response {
    match table_inner(app, req, method, segments, query).await {
        Ok(r) => cors(r),
        Err(e) => err_resp(e),
    }
}

async fn table_inner(
    app: Arc<crate::App>,
    req: Request,
    method: Method,
    segments: Vec<String>,
    _query: Option<String>,
) -> Result<Response, ApiError> {
    let ri = req_info(&req);
    let range_in = ri.range.clone();
    let ctx = prepare(&app, ri, &method).await?;
    let t = resolve_table(&ctx.cache, ctx.schemas.as_slice(), &segments, ctx.profile.as_deref())?;

    // parse query grammar
    let mut q = Query {
        select: Vec::new(),
        conds: Vec::new(),
        order: Vec::new(),
        limit: None,
        offset: None,
        range: None,
    };
    for (k, v) in &ctx.params {
        match k.as_str() {
            "select" => q.select = crate::parser::parse_select(v)?,
            "order" => q.order = crate::parser::parse_order(v)?,
            "limit" => q.limit = Some(crate::parser::parse_nonneg(v, "limit")?),
            "offset" => q.offset = Some(crate::parser::parse_nonneg(v, "offset")?),
            "columns" | "on_conflict" => {}
            other if other.contains('.') || other == "and" || other == "or" || other.starts_with("not.") => {
                // possibly embed-scoped or logic; routed below
            }
            _ => { /* treated as filter below */ }
        }
    }
    q.conds = crate::parser::route_embed_params(&mut q.select, &ctx.params)?;
    crate::parser::apply_range(&mut q, range_in);

    let content_type = content_type_of(&req);
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|e| ApiError::body_required().with_details(e.to_string()))?;
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    let tx_end = match ctx.prefer.tx.as_deref() {
        Some("rollback") => TxEnd::Rollback,
        Some("commit") => TxEnd::Commit,
        _ => ctx.cfg.db_tx_end_default,
    };
    let exec_ctx = ExecCtx {
        role: ctx.role.clone(),
        claims: ctx.claims.clone(),
        method: method.to_string(),
        path: req_path(&segments),
        tx_end,
        pre_request: ctx.cfg.db_pre_request.clone(),
    };

    match method {
        Method::GET | Method::HEAD => read_response(&app, &ctx, t, q, exec_ctx).await,
        Method::POST => insert_response(&app, &ctx, t, &content_type, &body_str, exec_ctx, &segments, &q).await,
        Method::PATCH => update_response(&app, &ctx, t, &content_type, &body_str, q, exec_ctx).await,
        Method::PUT => put_response(&app, &ctx, t, &content_type, &body_str, q, exec_ctx).await,
        Method::DELETE => delete_response(&app, &ctx, t, q, exec_ctx).await,
        _ => Err(ApiError::new(405, "PGRST102", "method not allowed")
            .with_details("allowed: GET, HEAD, POST, PATCH, PUT, DELETE, OPTIONS")),
    }
}

fn content_type_of(req: &Request) -> String {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn req_path(segments: &[String]) -> String {
    match segments.len() {
        0 => "/".into(),
        1 => format!("/{}", segments[0]),
        _ => format!("/{}/{}", segments[0], segments[1]),
    }
}

fn count_mode(prefer: &Prefer) -> CountMode {
    match prefer.count.as_deref() {
        Some("exact") => CountMode::Exact,
        Some("planned") | Some("estimated") => CountMode::Estimated,
        _ => CountMode::None,
    }
}

async fn read_response(
    app: &Arc<crate::App>,
    ctx: &ReqCtx,
    t: &TableInfo,
    q: Query,
    exec: ExecCtx,
) -> Result<Response, ApiError> {
    // Range header implies an exact count (PostgREST semantics)
    let mut cm = count_mode(&ctx.prefer);
    if q.range.is_some() && cm == CountMode::None {
        cm = CountMode::Exact;
    }
    let read = compiler::compile_read(&ctx.cache, t, &q, cm, ctx.cfg.db_max_rows)?;

    let mut stmts: Vec<String> = Vec::new();
    if let Some(csql) = &read.count_sql {
        stmts.push(csql.clone());
    }
    stmts.push(read.rows_sql.clone());

    // PostgREST-Plan support (Tier 2): EXPLAIN on the rows statement
    if ctx.prefer.plan {
        stmts.push(format!("EXPLAIN (FORMAT TEXT) {}", read.rows_sql));
    }

    let out = pg::execute(&app.pool, &exec, &stmts).await?;

    // interpret results: [count?] [rows] [plan?]
    let mut idx = 0usize;
    let mut total: Option<i64> = None;
    if read.count_sql.is_some() {
        if let Some((cols, rows)) = out.results.get(idx) {
            // Estimated mode runs EXPLAIN (FORMAT JSON): extract "Plan Rows"
            if cm == CountMode::Estimated {
                let text: String = rows
                    .iter()
                    .filter_map(|r| r.first().cloned().flatten())
                    .collect::<Vec<_>>()
                    .join("");
                if let Some(p) = text.find("\"Plan Rows\":") {
                    let rest = &text[p + "\"Plan Rows\":".len()..];
                    let num: String = rest
                        .chars()
                        .skip_while(|c| c.is_whitespace())
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    total = num.parse::<i64>().ok();
                }
            } else if let Some(Some(v)) = rows.first().and_then(|r| {
                cols.iter().position(|c| c == "total").map(|i| r.get(i).cloned())
            }) {
                total = v.as_deref().and_then(|s| s.parse::<i64>().ok());
            }
        }
        idx += 1;
    }
    let (cols, rows) = out
        .results
        .get(idx)
        .cloned()
        .unwrap_or_else(|| (vec!["row".into()], Vec::new()));
    idx += 1;

    // aggregate select (no "row" col) -> object(s); GROUP BY can yield several
    let is_aggregate = !cols.is_empty() && cols[0] != "row";
    let json_rows: Vec<String> = if is_aggregate {
        rows.iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    let v = r.get(i).cloned().flatten();
                    let jv = match v {
                        None => serde_json::Value::Null,
                        Some(s) => serde_json::from_str(&s)
                            .unwrap_or(serde_json::Value::String(s.clone())),
                    };
                    m.insert(c.clone(), jv);
                }
                serde_json::Value::Object(m).to_string()
            })
            .collect()
    } else {
        rows.iter()
            .filter_map(|r| r.first().cloned().flatten())
            .map(compact_json)
            .collect()
    };

    let media = headers::negotiate_accept(ctx.accept_raw.as_deref())
        .map_err(|e| ApiError::new(406, "PGRST115", e))?;
    let shaped = media::shape(&json_rows, media)?;

    let mut hdrs: Vec<(&'static str, String)> = Vec::new();
    hdrs.push(("Content-Type", shaped.content_type.to_string()));

    // range / count semantics
    if !is_aggregate {
        let ranged = q.range.is_some() || q.limit.is_some() || q.offset.is_some();
        // 416: unsatisfiable range with known total
        if let (Some(r), Some(tv)) = (&q.range, total) {
            if tv > 0 && r.first >= tv {
                return Err(ApiError::invalid_range()
                    .with_details(format!("Content-Range: items */{}", tv)));
            }
        }
        let first = q.offset.unwrap_or(0);
        let spec = match total {
            Some(tv) => {
                if json_rows.is_empty() {
                    Some(format!("items */{}", tv))
                } else {
                    Some(format!("items {}-{}/{}", first, first + json_rows.len() as i64 - 1, tv))
                }
            }
            None => {
                if ranged && !json_rows.is_empty() {
                    Some(format!("items {}-{}/*", first, first + json_rows.len() as i64 - 1))
                } else {
                    None
                }
            }
        };
        if let Some(sp) = spec {
            hdrs.push(("Content-Range", sp));
        }
        let partial = match total {
            Some(tv) => first > 0 || (json_rows.len() as i64) < tv,
            None => ranged,
        };
        let status = if partial { 206 } else { 200 };
        if ranged {
            // canonical URL: original query string (PostgREST echoes it back)
            let mut loc = req_path_url(&segments_of(t));
            if let Some(qs) = &ctx.raw_query {
                if !qs.is_empty() {
                    loc.push('?');
                    loc.push_str(qs);
                }
            }
            hdrs.push(("Content-Location", loc));
        }
        if let Some(ap) = ctx.prefer.applied() {
            hdrs.push(("Preference-Applied", ap));
        }
        return finalize(app, ctx, out, idx, hdrs, shaped, status).await;
    }

    if let Some(ap) = ctx.prefer.applied() {
        hdrs.push(("Preference-Applied", ap));
    }
    finalize(app, ctx, out, idx, hdrs, shaped, 200).await
}

fn segments_of(t: &TableInfo) -> Vec<String> {
    vec![t.schema.clone(), t.name.clone()]
}

fn req_path_url(segments: &[String]) -> String {
    format!("/{}", segments.join("/"))
}

// attach plan header if requested
async fn finalize(
    app: &Arc<crate::App>,
    ctx: &ReqCtx,
    out: pg::ExecResult,
    idx_after_rows: usize,
    mut hdrs: Vec<(&'static str, String)>,
    shaped: media::Shaped,
    status: u16,
) -> Result<Response, ApiError> {
    if ctx.prefer.plan {
        if let Some((cols, rows)) = out.results.get(idx_after_rows) {
            let plan_col = cols.iter().position(|c| c.contains("QUERY PLAN") || c == "row");
            if let Some(pc) = plan_col {
                let text = rows
                    .iter()
                    .filter_map(|r| r.get(pc).cloned().flatten())
                    .collect::<Vec<_>>()
                    .join(" | ");
                hdrs.push(("PostgREST-Plan", text));
            }
        }
    }
    let _ = app;
    Ok(mk_response(status, hdrs, shaped.body))
}

fn parse_body_rows(content_type: &str, body: &str) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, ApiError> {
    let ct = content_type.split(';').next().unwrap_or("").trim();
    if body.trim().is_empty() {
        return Err(ApiError::body_required());
    }
    match ct {
        "application/json" | "application/vnd.pgrst.object+json" | "" => {
            let v: serde_json::Value = serde_json::from_str(body)
                .map_err(|e| ApiError::body_required().with_details(e.to_string()))?;
            match v {
                serde_json::Value::Array(items) => {
                    let mut rows = Vec::new();
                    for it in items {
                        match it {
                            serde_json::Value::Object(m) => rows.push(m),
                            _ => return Err(ApiError::body_required().with_details("array items must be objects")),
                        }
                    }
                    Ok(rows)
                }
                serde_json::Value::Object(m) => Ok(vec![m]),
                _ => Err(ApiError::body_required()),
            }
        }
        "text/csv" => {
            let table = sql::csv_parse(body).map_err(|e| ApiError::body_required().with_details(e))?;
            if table.len() < 2 {
                return Ok(Vec::new());
            }
            let header: Vec<String> = table[0].clone();
            let mut rows = Vec::new();
            for r in &table[1..] {
                let mut m = serde_json::Map::new();
                for (i, c) in header.iter().enumerate() {
                    let val = r.get(i).cloned().unwrap_or_default();
                    m.insert(c.clone(), serde_json::Value::String(val));
                }
                rows.push(m);
            }
            Ok(rows)
        }
        other => Err(ApiError::unsupported_media(other)),
    }
}

async fn insert_response(
    app: &Arc<crate::App>,
    ctx: &ReqCtx,
    t: &TableInfo,
    content_type: &str,
    body: &str,
    exec: ExecCtx,
    segments: &[String],
    q: &Query,
) -> Result<Response, ApiError> {
    let rows = parse_body_rows(content_type, body)?;
    let columns_opt: Option<Vec<String>> = ctx
        .params
        .iter()
        .find(|(k, _)| k == "columns")
        .map(|(_, v)| v.split(',').map(|s| s.trim().to_string()).collect());
    let on_conflict: Option<Vec<String>> = ctx
        .params
        .iter()
        .find(|(k, _)| k == "on_conflict")
        .map(|(_, v)| v.split(',').map(|s| s.trim().to_string()).collect());
    let resolution = match ctx.prefer.resolution.as_deref() {
        Some("merge-duplicates") => Resolution::Merge,
        Some("ignore-duplicates") => Resolution::Ignore,
        _ => Resolution::None,
    };
    let missing_default = ctx.prefer.missing.as_deref() == Some("default");
    let wants_rows = ctx.prefer.ret.as_deref() == Some("representation");

    let m = mutate::compile_insert(
        &ctx.cache,
        t,
        &rows,
        columns_opt.as_deref(),
        on_conflict.as_deref(),
        resolution,
        missing_default,
        wants_rows,
        &q.select,
    )?;
    let out = pg::execute(&app.pool, &exec, &[m.sql]).await?;

    let mut hdrs: Vec<(&'static str, String)> = Vec::new();
    if let Some(ap) = ctx.prefer.applied() {
        hdrs.push(("Preference-Applied", ap));
    }

    // rows are always returned; use them for Location, then shape per preference
    let (_cols, rws) = out.results.first().cloned().unwrap_or_else(|| (vec!["row".into()], Vec::new()));
    let json_rows: Vec<String> = rws
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .map(compact_json)
        .collect();

    if !t.pks.is_empty() && json_rows.len() == 1 {
        if let Some(v) = json_rows.first().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) {
            let mut pairs: Vec<String> = Vec::new();
            for pk in &t.pks {
                if let Some(pv) = v.get(pk) {
                    pairs.push(format!("{}=eq.{}", headers::encode_component(pk), headers::encode_component(&json_scalar_to_text(pv))));
                }
            }
            if !pairs.is_empty() {
                hdrs.push(("Location", format!("{}?{}", req_path_url(segments), pairs.join("&"))));
            }
        }
    }

    if !m.wants_rows {
        return Ok(mk_response(201, hdrs, String::new()));
    }

    let media = headers::negotiate_accept(ctx.accept_raw.as_deref()).map_err(|e| ApiError::new(406, "PGRST115", format!("Not acceptable: {}", e)))?;
    let shaped = media::shape(&json_rows, media)?;
    hdrs.push(("Content-Type", shaped.content_type.to_string()));
    Ok(mk_response(201, hdrs, shaped.body))
}

fn json_scalar_to_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Compact a JSON row document (PostgreSQL json_agg emits `[{...}, {...}]`
/// with spaces; PostgREST responses are compact). Non-JSON text passes through.
fn compact_json(s: String) -> String {
    if s.starts_with('{') || s.starts_with('[') {
        serde_json::from_str::<serde_json::Value>(&s)
            .map(|v| v.to_string())
            .unwrap_or(s)
    } else {
        s
    }
}

async fn update_response(
    app: &Arc<crate::App>,
    ctx: &ReqCtx,
    t: &TableInfo,
    content_type: &str,
    body: &str,
    q: Query,
    exec: ExecCtx,
) -> Result<Response, ApiError> {
    let ct = content_type.split(';').next().unwrap_or("").trim();
    if ct != "application/json" && !ct.is_empty() {
        return Err(ApiError::unsupported_media(ct));
    }
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ApiError::body_required().with_details(e.to_string()))?;
    let obj = match v {
        serde_json::Value::Object(m) => m,
        _ => return Err(ApiError::body_required().with_details("PATCH body must be a JSON object")),
    };
    let wants_rows = ctx.prefer.ret.as_deref() == Some("representation");
    let m = mutate::compile_update(&ctx.cache, t, &q, &obj, wants_rows, &q.select)?;
    let out = pg::execute(&app.pool, &exec, &[m.sql]).await?;

    let mut hdrs: Vec<(&'static str, String)> = Vec::new();
    if let Some(ap) = ctx.prefer.applied() {
        hdrs.push(("Preference-Applied", ap));
    }
    if !m.wants_rows {
        return Ok(mk_response(204, hdrs, String::new()));
    }
    let rws = out.results.first().map(|(_, r)| r.clone()).unwrap_or_default();
    let json_rows: Vec<String> = rws
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .map(compact_json)
        .collect();
    let media = headers::negotiate_accept(ctx.accept_raw.as_deref()).map_err(|e| ApiError::new(406, "PGRST115", format!("Not acceptable: {}", e)))?;
    let shaped = media::shape(&json_rows, media)?;
    hdrs.push(("Content-Type", shaped.content_type.to_string()));
    Ok(mk_response(200, hdrs, shaped.body))
}

async fn delete_response(
    app: &Arc<crate::App>,
    ctx: &ReqCtx,
    t: &TableInfo,
    q: Query,
    exec: ExecCtx,
) -> Result<Response, ApiError> {
    let wants_rows = ctx.prefer.ret.as_deref() == Some("representation");
    let m = mutate::compile_delete(&ctx.cache, t, &q, wants_rows, &q.select)?;
    let out = pg::execute(&app.pool, &exec, &[m.sql]).await?;

    let mut hdrs: Vec<(&'static str, String)> = Vec::new();
    if let Some(ap) = ctx.prefer.applied() {
        hdrs.push(("Preference-Applied", ap));
    }
    if !m.wants_rows {
        return Ok(mk_response(204, hdrs, String::new()));
    }
    let rws = out.results.first().map(|(_, r)| r.clone()).unwrap_or_default();
    let json_rows: Vec<String> = rws
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .map(compact_json)
        .collect();
    let media = headers::negotiate_accept(ctx.accept_raw.as_deref()).map_err(|e| ApiError::new(406, "PGRST115", format!("Not acceptable: {}", e)))?;
    let shaped = media::shape(&json_rows, media)?;
    hdrs.push(("Content-Type", shaped.content_type.to_string()));
    Ok(mk_response(200, hdrs, shaped.body))
}

/// PUT: upsert-by-primary-key (update-or-insert in one statement).
async fn put_response(
    app: &Arc<crate::App>,
    ctx: &ReqCtx,
    t: &TableInfo,
    content_type: &str,
    body: &str,
    q: Query,
    exec: ExecCtx,
) -> Result<Response, ApiError> {
    let ct = content_type.split(';').next().unwrap_or("").trim();
    if ct != "application/json" && !ct.is_empty() {
        return Err(ApiError::unsupported_media(ct));
    }
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ApiError::body_required().with_details(e.to_string()))?;
    let obj = match v {
        serde_json::Value::Object(m) => m,
        _ => return Err(ApiError::body_required().with_details("PUT body must be a JSON object")),
    };
    let wants_rows = ctx.prefer.ret.as_deref() == Some("representation");
    let m = mutate::compile_put(&ctx.cache, t, &q, &obj, wants_rows, &q.select)?;
    let out = pg::execute(&app.pool, &exec, &[m.sql]).await?;

    let mut hdrs: Vec<(&'static str, String)> = Vec::new();
    if let Some(ap) = ctx.prefer.applied() {
        hdrs.push(("Preference-Applied", ap));
    }
    if !m.wants_rows {
        return Ok(mk_response(204, hdrs, String::new()));
    }
    let rws = out.results.first().map(|(_, r)| r.clone()).unwrap_or_default();
    let json_rows: Vec<String> = rws
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .map(compact_json)
        .collect();
    let media = headers::negotiate_accept(ctx.accept_raw.as_deref()).map_err(|e| ApiError::new(406, "PGRST115", format!("Not acceptable: {}", e)))?;
    let shaped = media::shape(&json_rows, media)?;
    hdrs.push(("Content-Type", shaped.content_type.to_string()));
    Ok(mk_response(200, hdrs, shaped.body))
}

async fn rpc_handler(
    app: Arc<crate::App>,
    req: Request,
    method: Method,
    fname: String,
    query: Option<String>,
) -> Response {
    match rpc_inner(app, req, method, fname, query).await {
        Ok(r) => cors(r),
        Err(e) => err_resp(e),
    }
}

async fn rpc_inner(
    app: Arc<crate::App>,
    req: Request,
    method: Method,
    fname: String,
    _query: Option<String>,
) -> Result<Response, ApiError> {
    let ri = req_info(&req);
    let ctx = prepare(&app, ri, &method).await?;
    let candidates: Vec<crate::schema::FnInfo> =
        match ctx.cache.find_function(ctx.schemas.as_slice(), &fname) {
            Ok(f) => f.into_iter().cloned().collect(),
            Err(FindErr::Missing) => return Err(ApiError::not_found_relation("function", &fname)),
            Err(FindErr::Ambiguous) => return Err(ApiError::ambiguous("function", &fname)),
        };
    let cand_refs: Vec<&crate::schema::FnInfo> = candidates.iter().collect();

    // procedures are POST-only (CALL); volatile functions cannot run via GET
    let any_proc = candidates.iter().any(|f| f.is_procedure);
    let is_get = method == Method::GET || method == Method::HEAD;
    if is_get && any_proc {
        return Err(ApiError::new(
            405,
            "PGRST102",
            format!("procedure {} can only be called via POST", fname),
        ));
    }
    if is_get && candidates.iter().any(|f| f.volatile) {
        return Err(ApiError::new(
            405,
            "PGRST102",
            format!("function {} is VOLATILE and can only be called via POST", fname),
        ));
    }
    if !is_get && method != Method::POST {
        return Err(ApiError::new(405, "PGRST102", "method not allowed for RPC")
            .with_details("allowed: GET, POST, OPTIONS"));
    }

    // parse read-shaping from the query string (setof returns)
    let mut shape = compiler::RpcShape::default();
    let mut qselect: Vec<crate::ast::Sel> = Vec::new();
    let mut qorder: Vec<crate::ast::OrderItem> = Vec::new();
    let mut qlimit: Option<i64> = None;
    let mut qoffset: Option<i64> = None;
    let mut conds: Vec<crate::ast::Cond> = Vec::new();
    let setof = candidates.iter().any(|f| {
        matches!(&f.ret_kind, crate::schema::RetKind::SetOf(_, _) | crate::schema::RetKind::Table)
    });

    // args: POST body (or GET query params); conds: filter-shaped params
    let mut args_list: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();
    if method == Method::POST {
        let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
            .await
            .map_err(|_| ApiError::body_required())?;
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
        if !body_str.trim().is_empty() {
            let v: serde_json::Value = serde_json::from_str(&body_str)
                .map_err(|e| ApiError::body_required().with_details(e.to_string()))?;
            match v {
                serde_json::Value::Array(items) => {
                    // bulk only with Prefer: params=multiple-objects
                    if ctx.prefer.params.as_deref() != Some("multiple-objects") {
                        return Err(ApiError::new(
                            400,
                            "PGRST102",
                            "RPC body arrays require 'Prefer: params=multiple-objects'",
                        ));
                    }
                    for it in items {
                        match it {
                            serde_json::Value::Object(m) => args_list.push(m),
                            _ => return Err(ApiError::body_required()
                                .with_details("array items must be objects")),
                        }
                    }
                }
                serde_json::Value::Object(m) => args_list.push(m),
                _ => return Err(ApiError::body_required().with_details("RPC body must be a JSON object")),
            }
        }
    }
    if args_list.is_empty() {
        args_list.push(serde_json::Map::new());
    }
    // GET args come from the query string; on setof returns, filter-shaped
    // params (col.op.value) become conditions on the result instead.
    // POST also merges query-string params (body keys take precedence).
    for (k, v) in &ctx.params {
        match k.as_str() {
            "select" => qselect = crate::parser::parse_select(v)?,
            "order" => qorder = crate::parser::parse_order(v)?,
            "limit" => qlimit = Some(crate::parser::parse_nonneg(v, "limit")?),
            "offset" => qoffset = Some(crate::parser::parse_nonneg(v, "offset")?),
            "columns" | "on_conflict" => {}
            other => {
                let is_filter = setof
                    && crate::parser::split_op_value(v).is_some()
                    && !other.contains('.');
                if is_filter {
                    conds.push(crate::parser::parse_cond(other, v)?);
                } else {
                    for a in args_list.iter_mut() {
                        a.entry(other.to_string())
                            .or_insert_with(|| serde_json::Value::String(v.clone()));
                    }
                }
            }
        }
    }
    shape.select = qselect;
    shape.order = qorder;
    shape.limit = qlimit;
    shape.offset = qoffset;
    shape.conds = conds;
    // base table for embeds on setof results
    for f in &candidates {
        if let crate::schema::RetKind::SetOf(s, t) | crate::schema::RetKind::Composite(s, t) = &f.ret_kind {
            shape.base = ctx.cache.table(s, t).cloned();
            break;
        }
    }
    let use_shape = setof
        && args_list.len() == 1
        && (!shape.select.is_empty()
            || !shape.conds.is_empty()
            || !shape.order.is_empty()
            || shape.limit.is_some()
            || shape.offset.is_some());

    let rpc = mutate::compile_rpc(&ctx.cache, &cand_refs, &args_list, if use_shape { Some(&shape) } else { None })?;

    let tx_end = match ctx.prefer.tx.as_deref() {
        Some("rollback") => TxEnd::Rollback,
        Some("commit") => TxEnd::Commit,
        _ => ctx.cfg.db_tx_end_default,
    };
    let exec = ExecCtx {
        role: ctx.role.clone(),
        claims: ctx.claims.clone(),
        method: method.to_string(),
        path: format!("/rpc/{}", fname),
        tx_end,
        pre_request: ctx.cfg.db_pre_request.clone(),
    };
    let out = pg::execute(&app.pool, &exec, &[rpc.sql.clone()]).await?;

    let mut hdrs: Vec<(&'static str, String)> = Vec::new();
    if let Some(ap) = ctx.prefer.applied() {
        hdrs.push(("Preference-Applied", ap));
    }
    if !rpc.wants_rows {
        return Ok(mk_response(204, hdrs, String::new()));
    }
    let (cols, rws) = out.results.first().cloned().unwrap_or_else(|| (vec![], Vec::new()));
    if rpc.scalar_text {
        let shaped = media::shape_scalar_text(&rws, &cols);
        hdrs.push(("Content-Type", shaped.content_type.to_string()));
        return Ok(mk_response(200, hdrs, shaped.body));
    }
    let json_rows: Vec<String> = rws
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .map(compact_json)
        .collect();
    let media = headers::negotiate_accept(ctx.accept_raw.as_deref()).map_err(|e| ApiError::new(406, "PGRST115", format!("Not acceptable: {}", e)))?;
    // scalar / scalar-composite returns come back as one jsonb row -> object output
    let singular = candidates.iter().any(|f| {
        matches!(
            &f.ret_kind,
            crate::schema::RetKind::Scalar(_) | crate::schema::RetKind::Composite(_, _)
        )
    }) && args_list.len() == 1;
    let shaped = if singular {
        media::shape(&json_rows, OutMedia::Object)?
    } else {
        media::shape(&json_rows, media)?
    };
    hdrs.push(("Content-Type", shaped.content_type.to_string()));
    Ok(mk_response(200, hdrs, shaped.body))
}
