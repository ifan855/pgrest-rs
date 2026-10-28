// Execution layer: pooled connections, per-request transaction with role
// impersonation and claim GUCs, LISTEN/NOTIFY hot reload.
use crate::config::TxEnd;
use crate::error::ApiError;
use std::future::poll_fn;
use std::sync::Arc;

pub struct ExecResult {
    /// flattened results of all statements; each result is (columns, rows)
    pub results: Vec<(Vec<String>, Vec<Vec<Option<String>>>)>,
    pub notices: Vec<String>,
}

pub struct ExecCtx {
    pub role: String,
    pub claims: Option<serde_json::Value>,
    pub method: String,
    pub path: String,
    pub tx_end: TxEnd,
    pub pre_request: Option<String>,
}

pub async fn execute(
    pool: &deadpool_postgres::Pool,
    ctx: &ExecCtx,
    statements: &[String],
) -> Result<ExecResult, ApiError> {
    let mut client = pool.get().await.map_err(ApiError::from_pool)?;
    let tx = client.transaction().await.map_err(ApiError::from_db)?;
    let notices = Vec::new();

    // role + GUCs (SET LOCAL semantics: cleared at transaction end)
    tx.batch_execute(&format!(
        "SELECT set_config('role', {}, true)",
        crate::sql::ql(&ctx.role)
    ))
    .await
    .map_err(ApiError::from_db)?;
    let claims_json = match &ctx.claims {
        Some(c) => c.to_string(),
        None => "{}".to_string(),
    };
    tx.batch_execute(&format!(
        "SELECT set_config('request.jwt.claims', {}, true), \
                set_config('request.method', {}, true), \
                set_config('request.path', {}, true)",
        crate::sql::ql(&claims_json),
        crate::sql::ql(&ctx.method),
        crate::sql::ql(&ctx.path)
    ))
    .await
    .map_err(ApiError::from_db)?;

    // expose each top-level scalar claim as request.jwt.claim.<key> (PostgREST GUC convention)
    if let Some(serde_json::Value::Object(map)) = &ctx.claims {
        for (k, v) in map {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                other => other.to_string(),
            };
            if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue; // skip weird claim keys, GUC names are restricted
            }
            tx.batch_execute(&format!(
                "SELECT set_config('request.jwt.claim.{}', {}, true)",
                k,
                crate::sql::ql(&text)
            ))
            .await
            .map_err(ApiError::from_db)?;
        }
    }

    if let Some(pre) = &ctx.pre_request {
        tx.batch_execute(&format!("SELECT {}()", pre))
            .await
            .map_err(ApiError::from_db)?;
    }

    let mut results = Vec::new();
    for stmt in statements {
        if stmt.trim().is_empty() {
            continue;
        }
        let msgs = tx.simple_query(stmt).await.map_err(ApiError::from_db)?;
        let mut cols: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        for m in msgs {
            match m {
                tokio_postgres::SimpleQueryMessage::Row(r) => {
                    if cols.is_empty() {
                        cols = r.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    rows.push((0..r.columns().len())
                        .map(|i| r.get(i).map(|s| s.to_string()))
                        .collect());
                }
                tokio_postgres::SimpleQueryMessage::CommandComplete(_) => {}
                other => {
                    let _ = other;
                }
            }
        }
        results.push((cols, rows));
    }

    match ctx.tx_end {
        TxEnd::Commit => tx.commit().await.map_err(ApiError::from_db)?,
        TxEnd::Rollback => tx.rollback().await.map_err(ApiError::from_db)?,
    }

    Ok(ExecResult { results, notices })
}

/// Dedicated connection that LISTENs on `pgrst` and triggers reloads.
pub fn spawn_reload_listener(app: Arc<crate::App>) {
    tokio::spawn(async move {
        loop {
            let uri = {
                let cfg = app.cfg.read().await;
                cfg.db_uri.clone()
            };
            match tokio_postgres::connect(&uri, tokio_postgres::NoTls).await {
                Ok((client, mut connection)) => {
                    if let Err(e) = client.batch_execute("LISTEN pgrst").await {
                        eprintln!("[pgrest-rs] LISTEN failed: {}", e);
                    } else {
                        eprintln!("[pgrest-rs] listening for pgrst notifications");
                    }
                    while let Some(msg) = poll_fn(|cx| connection.poll_message(cx)).await {
                        if let Ok(tokio_postgres::AsyncMessage::Notification(n)) = msg {
                            match n.payload() {
                                "reload schema" => {
                                    eprintln!("[pgrest-rs] reload schema requested");
                                    reload_schema(&app).await;
                                }
                                "reload config" => {
                                    eprintln!("[pgrest-rs] reload config requested");
                                    reload_config(&app).await;
                                    reload_schema(&app).await;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[pgrest-rs] reload listener connect failed: {}", e);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

pub async fn reload_schema(app: &Arc<crate::App>) {
    let (pool, schemas) = {
        let cfg = app.cfg.read().await;
        (app.pool.clone(), cfg.db_schemas.clone())
    };
    match crate::schema::load(&pool, &schemas).await {
        Ok(cache) => {
            let mut w = app.cache.write().await;
            *w = Arc::new(cache);
            app.metrics.schema_reloads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[pgrest-rs] schema cache reloaded");
        }
        Err(e) => eprintln!("[pgrest-rs] schema reload failed: {} {}", e.code, e.message),
    }
}

pub async fn reload_config(app: &Arc<crate::App>) {
    if !app.cfg.read().await.db_config {
        return;
    }
    let sql = "SELECT name, setting FROM postgrest.config";
    let res = async {
        let client = app.pool.get().await.map_err(ApiError::from_pool)?;
        let rows = client.query(sql, &[]).await.map_err(ApiError::from_db)?;
        let mut out = Vec::new();
        for r in rows {
            let name: String = r.get(0);
            let setting: String = r.get(1);
            out.push((name, setting));
        }
        Ok::<_, ApiError>(out)
    }
    .await;
    if let Ok(rows) = res {
        let mut cfg = app.cfg.write().await;
        cfg.apply_config_table(rows);
    }
}
