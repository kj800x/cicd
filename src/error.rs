use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use std::fmt::Write;
use thiserror::Error;

/// Formats an error and its entire source chain with each error on a new line
///
/// This produces output like:
/// ```
/// Error message
///   Caused by: First cause
///   Caused by: Second cause
///   Caused by: Root cause
/// ```
pub fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut output = String::new();
    write!(&mut output, "{}", err).ok();

    let mut source = err.source();
    while let Some(err) = source {
        write!(&mut output, "\n  Caused by: {}", err).ok();
        source = err.source();
    }

    output
}

/// Formats an anyhow::Error with its full chain
pub fn format_anyhow_chain(err: &anyhow::Error) -> String {
    let mut output = String::new();

    // Get the chain iterator from anyhow
    let chain: Vec<_> = err.chain().collect();

    if let Some((first, rest)) = chain.split_first() {
        write!(&mut output, "{}", first).ok();
        for cause in rest {
            write!(&mut output, "\n  Caused by: {}", cause).ok();
        }
    }

    output
}

impl PartialEq for AppError {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

/// Central application error type
#[derive(Error, Debug)]
pub enum AppError {
    /// Database-related errors
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Database pool error: {0}")]
    DatabasePool(#[from] r2d2::Error),

    #[error("Database migration error: {0}")]
    DatabaseMigration(String),

    /// Kubernetes-related errors
    #[error("Kubernetes error: {0}")]
    Kubernetes(#[from] kube::Error),

    #[error("Kubernetes config error: {0}")]
    KubernetesConfig(String),

    #[error("Kubernetes resource not found: {0}")]
    KubernetesNotFound(String),

    /// Webhook-related errors
    #[error("Webhook error: {0}")]
    Webhook(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// Discord notification errors
    #[error("Discord error: {0}")]
    Discord(String),

    #[error("Serenity error: {0}")]
    Serenity(String),

    /// HTTP client errors
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    /// GraphQL errors
    #[error("GraphQL error: {0}")]
    GraphQL(String),

    /// Serialization/Deserialization errors
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Environment variable error: {0}")]
    EnvVar(#[from] std::env::VarError),

    /// Parsing errors
    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Chrono parse error: {0}")]
    ChronoParse(#[from] chrono::ParseError),

    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Generic internal errors with context
    #[error("Internal error: {0}")]
    Internal(String),

    /// Not found errors
    #[error("Not found: {0}")]
    NotFound(String),

    /// Invalid input errors
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

/// Convenience type alias for Results using AppError
pub type AppResult<T> = Result<T, AppError>;

impl ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        log::error!("HTTP error response: {}", self);

        let status_code = self.status_code();
        let error_message = self.to_string();

        // For internal errors, include the full error chain in the response
        // since this is an internal-only application
        let body = serde_json::json!({
            "error": error_message,
            "status": status_code.as_u16(),
        });

        HttpResponse::build(status_code)
            .content_type("application/json")
            .json(body)
    }

    fn status_code(&self) -> StatusCode {
        match self {
            AppError::Database(_)
            | AppError::DatabasePool(_)
            | AppError::DatabaseMigration(_)
            | AppError::Kubernetes(_)
            | AppError::KubernetesConfig(_)
            | AppError::WebSocket(_)
            | AppError::Discord(_)
            | AppError::Serenity(_)
            | AppError::GraphQL(_)
            | AppError::Json(_)
            | AppError::Yaml(_)
            | AppError::Config(_)
            | AppError::EnvVar(_)
            | AppError::ChronoParse(_)
            | AppError::Io(_)
            | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,

            AppError::NotFound(_) | AppError::KubernetesNotFound(_) => StatusCode::NOT_FOUND,

            AppError::InvalidInput(_) | AppError::Parse(_) => StatusCode::BAD_REQUEST,

            AppError::Webhook(_) | AppError::Http(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

// Implement From for common error types that don't have automatic conversion
impl From<String> for AppError {
    fn from(s: String) -> Self {
        AppError::Internal(s)
    }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        AppError::Internal(s.to_string())
    }
}

// Helper for converting octocrab errors
impl From<octocrab::Error> for AppError {
    fn from(e: octocrab::Error) -> Self {
        AppError::Internal(format!("Octocrab error: {}", e))
    }
}

// Helper for converting serde_variant errors
// Note: serde_variant::Error is not a standard error type, so we convert via string
impl From<Box<dyn std::error::Error>> for AppError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        AppError::Internal(e.to_string())
    }
}
