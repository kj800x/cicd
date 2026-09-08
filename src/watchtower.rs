//! The watchtower client: the one place cicd talks to the tag feed.
//!
//! Watchtower is a dedicated service that polls container registries and
//! keeps every image's tags with digest history. cicd registers the images
//! its tag parameters name, resolves "latest matching" through it, and
//! follows its event feed to autodeploy. Nothing else reads watchtower.
//!
//! Only a tracked tag parameter needs it: pins, rollbacks, undeploys and
//! every commit or value parameter never call here. When it is down,
//! resolution fails closed with [`AppError::Unavailable`] and the person
//! types the tag for that one deploy instead (see `deploys`).

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::kubernetes::parameters::ImageRef;

/// In-cluster service by default; override for local development.
pub const DEFAULT_URL: &str = "http://watchtower.cicd.svc";

#[derive(Clone)]
pub struct Watchtower {
    base: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Repo {
    pub id: u64,
    pub registry: String,
    pub name: String,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TagDigest {
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tag {
    pub tag: String,
    /// Still present in the registry's tag list.
    pub active: bool,
    /// Newest first; empty until the digest has been fetched.
    #[serde(default)]
    pub history: Vec<TagDigest>,
}

impl Tag {
    pub fn digest(&self) -> Option<&str> {
        self.history.first().map(|h| h.digest.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HydratedRepo {
    pub id: u64,
    pub registry: String,
    pub name: String,
    pub active: bool,
    #[serde(default)]
    pub tag: Vec<Tag>,
    pub last_checked_at: Option<String>,
    pub last_error: Option<String>,
}

/// What happened to a tag, as watchtower's feed names it.
#[allow(dead_code)] // consumed by the event poller in a later change
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum EventKind {
    #[serde(rename = "tag_added")]
    Added,
    #[serde(rename = "tag_moved")]
    Moved,
    #[serde(rename = "tag_removed")]
    Removed,
}

#[allow(dead_code)] // consumed by the event poller in a later change
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub id: u64,
    pub registry: String,
    pub name: String,
    pub tag: String,
    pub kind: EventKind,
    pub digest: Option<String>,
    pub previous_digest: Option<String>,
}

#[allow(dead_code)] // consumed by the event poller in a later change
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventsPage {
    pub events: Vec<Event>,
    pub latest_id: u64,
}

static GLOBAL: OnceLock<Watchtower> = OnceLock::new();

impl Watchtower {
    pub fn new(base: &str) -> Self {
        Watchtower {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    /// The process-wide client, configured from `WATCHTOWER_URL` on first use.
    pub fn global() -> &'static Watchtower {
        GLOBAL.get_or_init(|| {
            let url = std::env::var("WATCHTOWER_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
            Watchtower::new(&url)
        })
    }

    fn unavailable(&self, e: reqwest::Error) -> AppError {
        AppError::Unavailable(format!("watchtower at {} is unreachable: {e}", self.base))
    }

    /// The image's repo with its tags, or `None` when watchtower does not
    /// know the image (register it first).
    pub async fn lookup(&self, image: &ImageRef) -> AppResult<Option<HydratedRepo>> {
        let response = self
            .http
            .get(format!("{}/api/lookup", self.base))
            .query(&[("registry", &image.registry), ("name", &image.name)])
            .send()
            .await
            .map_err(|e| self.unavailable(e))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let repo = response
            .error_for_status()
            .map_err(|e| self.unavailable(e))?
            .json::<HydratedRepo>()
            .await
            .map_err(|e| self.unavailable(e))?;
        Ok(Some(repo))
    }

    #[allow(dead_code)] // registration reconcile, next change
    pub async fn list_repos(&self) -> AppResult<Vec<Repo>> {
        self.http
            .get(format!("{}/api/repo", self.base))
            .send()
            .await
            .map_err(|e| self.unavailable(e))?
            .error_for_status()
            .map_err(|e| self.unavailable(e))?
            .json()
            .await
            .map_err(|e| self.unavailable(e))
    }

    /// Register an image (or re-activate it) and ask for an immediate
    /// refresh so its tags are usable within seconds.
    pub async fn register(&self, image: &ImageRef) -> AppResult<Repo> {
        let repo: Repo = self
            .http
            .post(format!("{}/api/repo", self.base))
            .json(&serde_json::json!({ "registry": image.registry, "name": image.name }))
            .send()
            .await
            .map_err(|e| self.unavailable(e))?
            .error_for_status()
            .map_err(|e| self.unavailable(e))?
            .json()
            .await
            .map_err(|e| self.unavailable(e))?;
        // Best effort: the scheduler refreshes it within a minute anyway.
        let _ = self
            .http
            .post(format!("{}/api/repo/{}/refresh", self.base, repo.id))
            .send()
            .await;
        Ok(repo)
    }

    #[allow(dead_code)] // registration reconcile, next change
    pub async fn set_active(&self, id: u64, active: bool) -> AppResult<()> {
        self.http
            .post(format!("{}/api/repo/{id}/active", self.base))
            .json(&active)
            .send()
            .await
            .map_err(|e| self.unavailable(e))?
            .error_for_status()
            .map_err(|e| self.unavailable(e))?;
        Ok(())
    }

    #[allow(dead_code)] // event poller, later change
    pub async fn events(&self, after: u64, limit: usize) -> AppResult<EventsPage> {
        self.http
            .get(format!("{}/api/events", self.base))
            .query(&[("after", after.to_string()), ("limit", limit.to_string())])
            .send()
            .await
            .map_err(|e| self.unavailable(e))?
            .error_for_status()
            .map_err(|e| self.unavailable(e))?
            .json()
            .await
            .map_err(|e| self.unavailable(e))
    }
}
