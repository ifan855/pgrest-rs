// pgrest-rs: a lean PostgREST-compatible API server for PostgreSQL.
mod ast;
mod auth;
mod compiler;
mod config;
mod error;
mod handlers;
mod headers;
mod media;
mod metrics;
mod mutate;
mod openapi;
mod parser;
mod pg;
mod schema;
mod sql;

use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct App {
    pub pool: deadpool_postgres::Pool,
    pub cache: RwLock<Arc<schema::SchemaCache>>,
    pub cfg: RwLock<config::Config>,
    pub metrics: metrics::Metrics,
    pub jwks: tokio::sync::RwLock<auth::JwksCache>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut cfg = config::Config::from_env();

    // pool
    let pgcfg = tokio_postgres::Config::from_str(&cfg.db_uri).expect("invalid PGRST_DB_URI");
    let mgr = deadpool_postgres::Manager::new(pgcfg, tokio_postgres::NoTls);
    let pool = deadpool_postgres::Pool::builder(mgr)
        .max_size(cfg.db_pool)
        .build()
        .expect("pool build");

    // config table overlay (Tier 2)
    if cfg.db_config {
        let sql = "SELECT name, setting FROM postgrest.config";
        if let Ok(client) = pool.get().await {
            if let Ok(rows) = client.query(sql, &[]).await {
                let mut kv = Vec::new();
                for r in rows {
                    let name: String = r.get(0);
                    let setting: String = r.get(1);
                    kv.push((name, setting));
                }
                cfg.apply_config_table(kv);
                // pool size may have changed: rebuild only if different
            }
        }
    }

    let app = Arc::new(App {
        pool: pool.clone(),
        cache: RwLock::new(Arc::new(schema::SchemaCache::default())),
        cfg: RwLock::new(cfg.clone()),
        metrics: metrics::Metrics::new(),
        jwks: RwLock::new({
            let mut j = auth::JwksCache::new();
            if let Some(u) = &cfg.jwks_url {
                j.set_url(u.clone());
            }
            j
        }),
    });

    // initial schema cache
    match schema::load(&pool, &cfg.db_schemas.clone()).await {
        Ok(cache) => {
            let n = cache.tables.len();
            let f = cache.functions.len();
            *app.cache.write().await = Arc::new(cache);
            eprintln!(
                "[pgrest-rs] schema cache loaded: {} relations, {} functions, schemas={:?}",
                n, f, cfg.db_schemas
            );
        }
        Err(e) => {
            eprintln!(
                "[pgrest-rs] FATAL: schema cache load failed ({}): {} {}",
                e.status, e.code, e.message
            );
            std::process::exit(1);
        }
    }

    pg::spawn_reload_listener(app.clone());

    let addr = format!("{}:{}", cfg.server_host, cfg.server_port);
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    eprintln!(
        "[pgrest-rs] v{} listening on http://{} (pid {})",
        env!("CARGO_PKG_VERSION"),
        addr,
        std::process::id()
    );

    let router = axum::Router::new()
        .route("/", axum::routing::any(handlers::entry))
        .route("/*path", axum::routing::any(handlers::entry))
        .with_state(app);

    axum::serve(listener, router).await.expect("server");
}
