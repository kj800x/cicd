use serenity::async_trait;

use crate::webhooks::models::{
    CheckRunEvent, CheckSuiteEvent, DeleteEvent, InstallationRepositoriesEvent, PushEvent,
    RepositoryEvent,
};

pub mod autodeploy;
pub mod config_sync;
pub mod database;
pub mod log;
pub mod manager;
pub mod metrics;
pub mod models;
pub mod recent;
pub mod tag_events;
pub mod util;

#[async_trait]
pub trait WebhookHandler {
    async fn handle_push(&self, __event: PushEvent) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn handle_check_run(&self, __event: CheckRunEvent) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn handle_check_suite(&self, __event: CheckSuiteEvent) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn handle_delete(&self, __event: DeleteEvent) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn handle_repository(&self, __event: RepositoryEvent) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn handle_installation_repositories(
        &self,
        __event: InstallationRepositoriesEvent,
    ) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn handle_unknown(&self, __event_type: &str) -> Result<(), anyhow::Error> {
        Ok(())
    }
}
