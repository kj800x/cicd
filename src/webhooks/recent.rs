//! In-memory ring buffer of recently received webhook events, for debugging.
//!
//! Nothing here is persisted; the buffer is reset on restart.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::webhooks::models::WebhookEvent;

/// Maximum number of events retained.
pub const CAPACITY: usize = 20;

#[derive(Debug, Clone)]
pub struct RecordedWebhook {
    /// Monotonic id, unique for the lifetime of the process.
    pub id: u64,
    pub received_at: DateTime<Utc>,
    pub event_type: String,
    pub payload: serde_json::Value,
}

impl RecordedWebhook {
    /// A short, human-readable one-liner describing the event, e.g.
    /// `push to kj800x/my-repo (main)` or `check_run completed · kj800x/my-repo · build`.
    pub fn summary(&self) -> String {
        let p = &self.payload;
        let repo = p
            .pointer("/repository/full_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let action = p.get("action").and_then(|v| v.as_str());

        match self.event_type.as_str() {
            "push" => {
                let git_ref = p.get("ref").and_then(|v| v.as_str()).map(|r| {
                    r.trim_start_matches("refs/heads/")
                        .trim_start_matches("refs/tags/")
                });
                let mut s = String::from("push");
                if let Some(repo) = &repo {
                    s.push_str(&format!(" to {}", repo));
                }
                if let Some(r) = git_ref {
                    s.push_str(&format!(" ({})", r));
                }
                if p.get("deleted").and_then(|v| v.as_bool()) == Some(true) {
                    s.push_str(" [deleted]");
                }
                s
            }
            "delete" => {
                let git_ref = p.get("ref").and_then(|v| v.as_str());
                let mut s = String::from("delete");
                if let Some(r) = git_ref {
                    s.push_str(&format!(" {}", r));
                }
                if let Some(repo) = &repo {
                    s.push_str(&format!(" in {}", repo));
                }
                s
            }
            "check_run" | "check_suite" => {
                let mut parts: Vec<String> = vec![self.event_type.clone()];
                if let Some(a) = action {
                    parts[0] = format!("{} {}", self.event_type, a);
                }
                if let Some(repo) = &repo {
                    parts.push(repo.clone());
                }
                if let Some(name) = p.pointer("/check_run/name").and_then(|v| v.as_str()) {
                    parts.push(name.to_string());
                }
                if let Some(branch) = p
                    .pointer("/check_suite/head_branch")
                    .and_then(|v| v.as_str())
                {
                    parts.push(branch.to_string());
                }
                if let Some(c) = p
                    .pointer("/check_run/conclusion")
                    .or_else(|| p.pointer("/check_suite/conclusion"))
                    .and_then(|v| v.as_str())
                {
                    parts.push(c.to_string());
                }
                parts.join(" · ")
            }
            "installation_repositories" => {
                let added = p
                    .get("repositories_added")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let removed = p
                    .get("repositories_removed")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!(
                    "installation_repositories {} · +{} / -{}",
                    action.unwrap_or(""),
                    added,
                    removed
                )
                .trim()
                .to_string()
            }
            other => {
                let mut s = other.to_string();
                if let Some(a) = action {
                    s.push_str(&format!(" {}", a));
                }
                if let Some(repo) = &repo {
                    s.push_str(&format!(" · {}", repo));
                }
                s
            }
        }
    }
}

struct Store {
    next_id: u64,
    events: VecDeque<RecordedWebhook>,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| {
        Mutex::new(Store {
            next_id: 1,
            events: VecDeque::with_capacity(CAPACITY + 1),
        })
    })
}

/// Record a received webhook event, evicting the oldest if over capacity.
pub fn record(event: &WebhookEvent) {
    let Ok(mut s) = store().lock() else {
        log::warn!("Recent webhook store lock poisoned; not recording event");
        return;
    };
    let id = s.next_id;
    s.next_id += 1;
    s.events.push_front(RecordedWebhook {
        id,
        received_at: Utc::now(),
        event_type: event.event_type.clone(),
        payload: event.payload.clone(),
    });
    while s.events.len() > CAPACITY {
        s.events.pop_back();
    }
}

/// All retained events, newest first.
pub fn list() -> Vec<RecordedWebhook> {
    store()
        .lock()
        .map(|s| s.events.iter().cloned().collect())
        .unwrap_or_default()
}

/// Look up a single retained event by id.
pub fn get(id: u64) -> Option<RecordedWebhook> {
    store()
        .lock()
        .ok()
        .and_then(|s| s.events.iter().find(|e| e.id == id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(event_type: &str, payload: serde_json::Value) -> RecordedWebhook {
        RecordedWebhook {
            id: 0,
            received_at: Utc::now(),
            event_type: event_type.to_string(),
            payload,
        }
    }

    #[test]
    fn push_summary() {
        let e = ev(
            "push",
            json!({"ref": "refs/heads/main", "repository": {"full_name": "kj800x/my-repo"}}),
        );
        assert_eq!(e.summary(), "push to kj800x/my-repo (main)");
    }

    #[test]
    fn check_run_summary() {
        let e = ev(
            "check_run",
            json!({
                "action": "completed",
                "repository": {"full_name": "kj800x/my-repo"},
                "check_run": {"name": "build", "conclusion": "success"}
            }),
        );
        assert_eq!(
            e.summary(),
            "check_run completed · kj800x/my-repo · build · success"
        );
    }

    #[test]
    fn unknown_summary_without_repo() {
        let e = ev("ping", json!({}));
        assert_eq!(e.summary(), "ping");
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        for i in 0..(CAPACITY + 5) {
            record(&WebhookEvent {
                event_type: format!("t{}", i),
                payload: json!({}),
            });
        }
        let all = list();
        assert_eq!(all.len(), CAPACITY);
        // newest first
        assert_eq!(all[0].event_type, format!("t{}", CAPACITY + 4));
        assert!(get(all[0].id).is_some());
        assert!(all.windows(2).all(|w| w[0].id > w[1].id));
    }
}
