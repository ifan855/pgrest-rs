// PGRST-style API errors: every failure becomes a JSON body with a code.
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Option<String>,
    pub hint: Option<String>,
}

impl ApiError {
    /// Map a deadpool error (pool exhaustion, backend error, timeout).
    pub fn from_pool(e: deadpool_postgres::PoolError) -> Self {
        match e {
            deadpool_postgres::PoolError::Backend(be) => ApiError::from_db(be),
            deadpool_postgres::PoolError::Timeout(_) => ApiError::new(503, "PGRST900", "connection pool timeout: all connections busy"),
            deadpool_postgres::PoolError::Closed => ApiError::new(503, "PGRST900", "connection pool closed"),
            deadpool_postgres::PoolError::NoRuntimeSpecified => ApiError::internal("pool runtime not specified"),
            other => ApiError::internal(other.to_string()),
        }
    }

    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError { status, code: code.into(), message: message.into(), details: None, hint: None }
    }

    pub fn with_details(mut self, d: impl Into<String>) -> Self {
        self.details = Some(d.into());
        self
    }

    pub fn with_hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }

    pub fn parse(msg: impl Into<String>) -> Self {
        Self::new(400, "PGRST100", format!("could not parse request: {}", msg.into()))
    }

    pub fn not_found_relation(what: &str, name: &str) -> Self {
        Self::new(
            404,
            if what == "function" { "PGRST202" } else { "PGRST118" },
            match what {
                "function" => format!("Could not find the function {} in the schema cache", name),
                "schema" => format!("Could not find the schema {} in the schema cache", name),
                _ => format!("Could not find the table '{}' in the schema cache", name),
            },
        )
    }

    pub fn ambiguous(what: &str, name: &str) -> Self {
        Self::new(
            300,
            if what == "function" { "PGRST203" } else { "PGRST201" },
            format!(
                "Could not choose the best candidate {} between: {}. Try to rename the ambiguous or add a hint.",
                if what == "function" { "function" } else { "relationship" },
                name
            ),
        )
    }

    pub fn body_required() -> Self {
        Self::new(400, "PGRST105", "an invalid request body was sent to PostgREST")
    }

    pub fn filters_required() -> Self {
        Self::new(400, "PGRST102", "Filters must be provided for update and delete operations")
    }

    pub fn invalid_range() -> Self {
        Self::new(
            416,
            "PGRST103",
            "Requested range not satisfiable",
        )
    }

    pub fn unsupported_media(ct: &str) -> Self {
        Self::new(415, "PGRST107", format!("Invalid request content-type: '{}'", ct))
    }

    pub fn unauthorized() -> Self {
        Self::new(401, "PGRST301", "JWT invalid")
    }

    pub fn multiple_objects() -> Self {
        Self::new(
            406,
            "PGRST116",
            "JSON object requested, multiple (or no) rows returned",
        )
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(500, "PGRST999", msg.into())
    }

    pub fn not_implemented(what: &str) -> Self {
        Self::new(501, "PGRST900", format!("Not implemented in pgrest-rs: {}", what))
    }

    // Map a Postgres error to the closest PGRST semantics.
    pub fn from_db(e: tokio_postgres::Error) -> Self {
        if let Some(db) = e.as_db_error() {
            let code = db.code().code();
            let msg = db.message().to_string();
            let mut err = match code {
                "23505" => Self::new(409, "PG23505", msg),          // unique_violation
                "23503" => Self::new(409, "PG23503", msg),          // foreign_key_violation
                "23514" => Self::new(400, "PG23514", msg),          // check_violation
                "23502" => Self::new(400, "PG23502", msg),          // not_null_violation
                "42501" => Self::new(403, "PG42501", msg)          // insufficient_privilege
                    .with_hint("If this happened through RLS, the role may lack a GRANT or a policy is blocking the row"),
                "42P01" => Self::new(404, "PGRST205", msg),         // undefined_table
                "42P07" => Self::new(409, "PG42P07", msg),
                "42883" => Self::new(404, "PGRST202", msg),         // undefined_function
                "42704" => Self::new(401, "PG42704", msg),          // undefined_object (e.g. role)
                "28000" => Self::new(401, "PG28000", msg),          // invalid_authorization
                "22P02" | "22007" | "22008" | "22P03" | "2200N" => {
                    Self::new(400, format!("PG{}", code), msg)
                }
                "42601" | "42P18" | "42804" | "42702" | "42703" => {
                    Self::new(400, format!("PG{}", code), msg)
                }
                "57014" => Self::new(408, "PG57014", msg),          // statement timeout
                "54001" => Self::new(500, "PG54001", msg),          // program_limit_exceeded
                "08P01" | "26000" | "XX000" => Self::new(500, format!("PG{}", code), msg),
                _ => Self::new(400, format!("PG{}", code), msg),
            };
            err.details = db.detail().map(|s| s.to_string());
            err.hint = db.hint().map(|s| s.to_string());
            err
        } else {
            Self::internal(e.to_string())
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        use axum::http::header;
        let body = serde_json::json!({
            "code": self.code,
            "message": self.message,
            "details": self.details,
            "hint": self.hint,
        });
        let status =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            body.to_string(),
        )
            .into_response()
    }
}
