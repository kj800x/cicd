#![allow(unused)]
use serenity::async_trait;

use crate::webhooks::{
    models::{CheckRunEvent, CheckSuiteEvent, DeleteEvent, PushEvent},
    WebhookHandler,
};

pub struct LogHandler {}

impl LogHandler {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl WebhookHandler for LogHandler {
    async fn handle_push(&self, event: PushEvent) -> Result<(), anyhow::Error> {
        log::info!("Received push event:\n{:#?}", event);
        Ok(())
    }

    async fn handle_check_run(&self, event: CheckRunEvent) -> Result<(), anyhow::Error> {
        log::info!("Received check run event:\n{:#?}", event);
        Ok(())
    }

    async fn handle_check_suite(&self, event: CheckSuiteEvent) -> Result<(), anyhow::Error> {
        log::info!("Received check suite event:\n{:#?}", event);
        Ok(())
    }

    async fn handle_delete(&self, event: DeleteEvent) -> Result<(), anyhow::Error> {
        log::info!("Received delete event:\n{:#?}", event);
        Ok(())
    }

    async fn handle_unknown(&self, event_type: &str) -> Result<(), anyhow::Error> {
        log::info!("Received unknown event: {}", event_type);
        Ok(())
    }
}
