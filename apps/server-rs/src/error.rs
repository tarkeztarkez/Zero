use serde_json::{Value, json};

/// Errors surfaced to the frontend as tRPC errors.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotImplemented(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(e.into())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::BadRequest(format!("invalid input: {e}"))
    }
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn trpc_code(&self) -> (&'static str, i32, u16) {
        match self {
            AppError::Unauthorized => ("UNAUTHORIZED", -32001, 401),
            AppError::NotFound(_) => ("NOT_FOUND", -32004, 404),
            AppError::BadRequest(_) => ("BAD_REQUEST", -32600, 400),
            AppError::NotImplemented(_) => ("METHOD_NOT_SUPPORTED", -32005, 405),
            AppError::Internal(_) => ("INTERNAL_SERVER_ERROR", -32603, 500),
        }
    }

    pub fn to_trpc(&self, path: &str) -> (Value, u16) {
        let (code, num, status) = self.trpc_code();
        let message = match self {
            AppError::Internal(e) => {
                tracing::error!(path, error = ?e, "procedure failed");
                format!("{e:#}")
            }
            other => other.to_string(),
        };
        (
            json!({
                "error": {
                    "json": {
                        "message": message,
                        "code": num,
                        "data": { "code": code, "httpStatus": status, "path": path }
                    }
                }
            }),
            status,
        )
    }
}

impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (code, _, status) = self.trpc_code();
        let status = axum::http::StatusCode::from_u16(status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        if let AppError::Internal(e) = &self {
            tracing::error!(error = ?e, "request failed");
        }
        (status, axum::Json(json!({ "code": code, "message": self.to_string() }))).into_response()
    }
}
