use serenity::async_trait;

use crate::webhooks::models::{CheckRunEvent, CheckSuiteEvent, DeleteEvent, PushEvent};

pub mod database;
pub mod log;
pub mod manager;
pub mod models;
pub mod util;

#[async_trait]
pub trait WebhookHandler {
    async fn handle_push(&self, event: PushEvent) -> Result<(), anyhow::Error>;
    async fn handle_check_run(&self, event: CheckRunEvent) -> Result<(), anyhow::Error>;
    async fn handle_check_suite(&self, event: CheckSuiteEvent) -> Result<(), anyhow::Error>;
    async fn handle_delete(&self, event: DeleteEvent) -> Result<(), anyhow::Error>;
    async fn handle_unknown(&self, event_type: &str) -> Result<(), anyhow::Error>;
}
