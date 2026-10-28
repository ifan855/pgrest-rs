// Configuration: PostgREST-compatible env vars, optionally overlaid by a
// `postgrest.config` table and hot-reloaded via `NOTIFY pgrst, 'reload config'`.
#[derive(Debug, Clone)]
pub struct Config {
    pub db_uri: String,
    pub db_schemas: Vec<String>,
    pub db_anon_role: String,
    pub db_pool: usize,
    pub db_max_rows: Option<i64>,
    pub db_pre_request: Option<String>, // schema-qualified function name
    pub db_pre_config: Option<String>,
    pub db_config: bool, // read postgrest.config table
    pub jwt_secret: Option<Vec<u8>>,
    pub jwks_url: Option<String>,
    pub server_host: String,
    pub server_port: u16,
    pub db_tx_end_default: TxEnd,
    pub log_level: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxEnd {
    Commit,
    Rollback,
}

impl Config {
    pub fn from_env() -> Config {
        let schemas = envs(&["PGRST_DB_SCHEMAS", "PGRST_DB_SCHEMA"])
            .unwrap_or_else(|| "public".to_string())
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let c = Config {
            db_uri: envs(&["PGRST_DB_URI", "DATABASE_URL"]).unwrap_or_else(|| {
                "postgresql://postgres@127.0.0.1:5432/postgres".to_string()
            }),
            db_schemas: schemas,
            db_anon_role: envs(&["PGRST_DB_ANON_ROLE"]).unwrap_or_else(|| "anon".to_string()),
            db_pool: envs(&["PGRST_DB_POOL"])
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            db_max_rows: envs(&["PGRST_DB_MAX_ROWS"]).and_then(|v| v.parse().ok()),
            db_pre_request: envs(&["PGRST_DB_PRE_REQUEST"]),
            db_pre_config: envs(&["PGRST_DB_PRE_CONFIG"]),
            db_config: envs(&["PGRST_DB_CONFIG"]).map(|v| v == "true").unwrap_or(false),
            jwt_secret: envs(&["PGRST_JWT_SECRET"]).map(|s| s.into_bytes()),
            jwks_url: envs(&["PGRST_JWKS_URL"]),
            server_host: envs(&["PGRST_SERVER_HOST"]).unwrap_or_else(|| "0.0.0.0".into()),
            server_port: envs(&["PGRST_SERVER_PORT"])
                .and_then(|v| v.parse().ok())
                .unwrap_or(3000),
            db_tx_end_default: envs(&["PGRST_DB_TX_END"])
                .map(|v| if v == "rollback" { TxEnd::Rollback } else { TxEnd::Commit })
                .unwrap_or(TxEnd::Commit),
            log_level: envs(&["PGRST_LOG_LEVEL"]).unwrap_or_else(|| "info".into()),
        };
        if let Err(e) = tokio_postgres::Config::from_str(&c.db_uri) {
            panic!("Invalid PGRST_DB_URI: {}", e);
        }
        c
    }

    /// Overlay values from a `postgrest.config` table (name, setting).
    pub fn apply_config_table(&mut self, rows: Vec<(String, String)>) {
        for (name, setting) in rows {
            match name.as_str() {
                "db-schemas" | "db-schema" => {
                    let v: Vec<String> = setting
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !v.is_empty() {
                        self.db_schemas = v;
                    }
                }
                "db-anon-role" => self.db_anon_role = setting,
                "db-pool" => {
                    if let Ok(n) = setting.parse() {
                        self.db_pool = n;
                    }
                }
                "db-max-rows" => self.db_max_rows = setting.parse().ok(),
                "db-pre-request" => self.db_pre_request = if setting.is_empty() { None } else { Some(setting) },
                "db-pre-config" => self.db_pre_config = if setting.is_empty() { None } else { Some(setting) },
                "jwt-secret" => self.jwt_secret = Some(setting.into_bytes()),
                "jwks-url" => self.jwks_url = if setting.is_empty() { None } else { Some(setting) },
                "server-port" => {
                    if let Ok(n) = setting.parse() {
                        self.server_port = n;
                    }
                }
                _ => {} // unknown keys ignored, like PostgREST
            }
        }
    }
}

use std::str::FromStr;

fn envs(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}
