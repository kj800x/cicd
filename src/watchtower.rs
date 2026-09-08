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
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum EventKind {
    #[serde(rename = "tag_added")]
    Added,
    #[serde(rename = "tag_moved")]
    Moved,
    #[serde(rename = "tag_removed")]
    Removed,
}

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

/// How far cicd manages watchtower's repo list. `CICD_WATCHTOWER_RECONCILE`:
/// `full` (default) registers referenced images and deactivates the rest,
/// `activate-only` never deactivates, `off` leaves watchtower alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileMode {
    Full,
    ActivateOnly,
    Off,
}

impl ReconcileMode {
    pub fn from_env() -> Self {
        match std::env::var("CICD_WATCHTOWER_RECONCILE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" => ReconcileMode::Off,
            "activate-only" | "activate_only" => ReconcileMode::ActivateOnly,
            _ => ReconcileMode::Full,
        }
    }
}

/// What a reconcile would do, computed from the referenced images and
/// watchtower's current list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    pub register: Vec<ImageRef>,
    pub activate: Vec<Repo>,
    pub deactivate: Vec<Repo>,
}

impl ReconcilePlan {
    pub fn is_empty(&self) -> bool {
        self.register.is_empty() && self.activate.is_empty() && self.deactivate.is_empty()
    }
}

pub fn plan_reconcile(
    referenced: &std::collections::BTreeSet<ImageRef>,
    repos: &[Repo],
    mode: ReconcileMode,
) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();
    if mode == ReconcileMode::Off {
        return plan;
    }
    for image in referenced {
        match repos
            .iter()
            .find(|r| r.registry == image.registry && r.name == image.name)
        {
            None => plan.register.push(image.clone()),
            Some(repo) if !repo.active => plan.activate.push(repo.clone()),
            Some(_) => {}
        }
    }
    if mode == ReconcileMode::Full {
        for repo in repos.iter().filter(|r| r.active) {
            let referenced = referenced
                .iter()
                .any(|i| i.registry == repo.registry && i.name == repo.name);
            if !referenced {
                plan.deactivate.push(repo.clone());
            }
        }
    }
    plan
}

/// The images every DeployConfig's tag parameters name.
pub fn referenced_images(
    configs: &[crate::kubernetes::DeployConfig],
) -> std::collections::BTreeSet<ImageRef> {
    configs
        .iter()
        .flat_map(|c| c.spec.spec.parameters.values())
        .filter_map(|s| s.image_ref())
        .collect()
}

/// Make watchtower's repo list match the tag parameters across all
/// configs. Every step is logged; failures are returned but callers treat
/// them as advisory, since the next sync or the periodic pass retries.
pub async fn reconcile_registrations(
    watchtower: &Watchtower,
    client: &kube::Client,
) -> AppResult<ReconcilePlan> {
    let mode = ReconcileMode::from_env();
    if mode == ReconcileMode::Off {
        return Ok(ReconcilePlan::default());
    }
    let configs = crate::kubernetes::api::get_all_deploy_configs(client).await?;
    let referenced = referenced_images(&configs);
    let repos = watchtower.list_repos().await?;
    let plan = plan_reconcile(&referenced, &repos, mode);
    for image in &plan.register {
        let repo = watchtower.register(image).await?;
        log::info!(
            "watchtower: registered {}/{} (id {})",
            repo.registry,
            repo.name,
            repo.id
        );
    }
    for repo in &plan.activate {
        watchtower.set_active(repo.id, true).await?;
        log::info!("watchtower: re-activated {}/{}", repo.registry, repo.name);
    }
    for repo in &plan.deactivate {
        watchtower.set_active(repo.id, false).await?;
        log::info!(
            "watchtower: deactivated {}/{}; no tag parameter references it",
            repo.registry,
            repo.name
        );
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn repo(id: u64, name: &str, active: bool) -> Repo {
        Repo {
            id,
            registry: "docker.io".into(),
            name: name.into(),
            active,
        }
    }

    #[test]
    fn reconcile_registers_activates_and_deactivates_by_mode() {
        let referenced: BTreeSet<ImageRef> = ["docker.io/library/nginx", "library/redis"]
            .into_iter()
            .map(ImageRef::parse)
            .collect();
        let repos = vec![
            repo(1, "library/redis", false),
            repo(2, "library/postgres", true),
            repo(3, "library/busybox", false),
        ];
        let full = plan_reconcile(&referenced, &repos, ReconcileMode::Full);
        assert_eq!(
            full.register,
            vec![ImageRef::parse("docker.io/library/nginx")]
        );
        assert_eq!(
            full.activate.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            full.deactivate.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2],
            "only active unreferenced repos are deactivated"
        );
        let activate_only = plan_reconcile(&referenced, &repos, ReconcileMode::ActivateOnly);
        assert!(activate_only.deactivate.is_empty());
        assert_eq!(activate_only.register.len(), 1);
        assert!(plan_reconcile(&referenced, &repos, ReconcileMode::Off).is_empty());
    }
}
