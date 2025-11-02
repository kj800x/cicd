pub mod api;
pub mod controller;
pub mod deploy_config;
pub mod deploy_config_status_builder;
pub mod deploy_handlers;
pub mod repo;
pub mod spec_editing;
pub mod webhook_handlers;

pub use api::{apply, delete_dynamic_object, list_namespace_objects};
pub use deploy_config::{DeployConfig, DeployConfigStatus};
pub use deploy_config_status_builder::DeployConfigStatusBuilder;
pub use repo::Repository;
// pub use spec_editing::{ensure_labels, ensure_owner_reference, is_owned_by};

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
