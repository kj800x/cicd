use opentelemetry::KeyValue;
use serenity::async_trait;

use crate::{
    build_status::BuildStatus,
    webhooks::{
        models::{CheckRunEvent, PushEvent},
        util::{extract_branch_name, rfc3339_to_millis},
        WebhookHandler,
    },
};

#[derive(Default)]
pub struct MetricsHandler {}

impl MetricsHandler {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WebhookHandler for MetricsHandler {
    async fn handle_push(&self, payload: PushEvent) -> Result<(), anyhow::Error> {
        let Some(branch) = extract_branch_name(&payload.r#ref) else {
            return Ok(());
        };

        let count = payload.commits.len() as u64;
        if count > 0 {
            let repo_label = format!(
                "{}/{}",
                payload.repository.owner.login, payload.repository.name
            );
            crate::metrics::get().commits_observed.add(
                count,
                &[
                    KeyValue::new("repo", repo_label),
                    KeyValue::new("branch", branch),
                ],
            );
        }

        Ok(())
    }

    async fn handle_check_run(&self, payload: CheckRunEvent) -> Result<(), anyhow::Error> {
        let repo_label = format!(
            "{}/{}",
            payload.repository.owner.login, payload.repository.name
        );
        let run = &payload.check_run;
        // Label metrics by the run name (e.g. "build") rather than an opaque,
        // per-commit suite id. The name is stable across commits, which is what
        // makes per-check trends (e.g. duration over time) meaningful.
        let check_name = run.name.clone();

        if payload.action.as_str() == "created" {
            crate::metrics::get().builds_started.add(
                1,
                &[
                    KeyValue::new("repo", repo_label.clone()),
                    KeyValue::new("check_name", check_name.clone()),
                ],
            );
        }

        // Resolve metrics fire when the run reaches a terminal state.
        if run.status.as_str() == "completed" {
            let status_str: String = BuildStatus::from_conclusion(run.conclusion.as_deref()).into();

            crate::metrics::get().builds_resolved.add(
                1,
                &[
                    KeyValue::new("repo", repo_label.clone()),
                    KeyValue::new("check_name", check_name.clone()),
                    KeyValue::new("status", status_str.clone()),
                ],
            );

            // Duration straight from GitHub's own timestamps (no DB round-trip
            // and no dependency on handler ordering).
            if let (Some(start), Some(end)) = (
                rfc3339_to_millis(run.started_at.as_deref()),
                rfc3339_to_millis(run.completed_at.as_deref()),
            ) {
                if end > start {
                    let duration_secs = (end - start) as f64 / 1000.0;
                    crate::metrics::get().build_duration_seconds.record(
                        duration_secs,
                        &[
                            KeyValue::new("repo", repo_label),
                            KeyValue::new("check_name", check_name),
                            KeyValue::new("status", status_str),
                        ],
                    );
                }
            }
        }

        Ok(())
    }
}
