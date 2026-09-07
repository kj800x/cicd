pub mod api;
pub mod controller;
pub mod cr_writers;
pub mod deploy_config;
pub mod deploy_handlers;
pub mod parameters;
pub mod patches;
pub mod repo;
pub mod selections;
pub mod spec_editing;
pub mod test_mode;
pub mod webhook_handlers;

pub use api::{apply, delete_dynamic_object, ensure_namespace_exists, list_namespace_objects};
pub use deploy_config::DeployConfig;
pub use repo::Repository;

/// Error type for controller operations
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Kube API error
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    /// Database error
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// App error
    #[error("App error: {0}")]
    App(#[from] crate::error::AppError),

    /// Other errors
    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}
