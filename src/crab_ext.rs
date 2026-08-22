//! GitHub API clients, authenticated as a GitHub App.
//!
//! The App itself (JWT auth) can only talk to `/app/*` routes. Everything
//! repo-shaped goes through an *installation* of the App on a user or org
//! account, so each installation gets its own [`Octocrab`] that mints and
//! refreshes short-lived installation tokens on demand.
//!
//! Installations are listed from `GET /app/installations` at startup and
//! again at the start of every owner-wide scan. A lookup for an account we
//! have not seen yet falls through to `GET /repos/{owner}/{repo}/installation`,
//! so installing the App somewhere new takes effect without a restart.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use jsonwebtoken::EncodingKey;
use octocrab::models::{AppId, Installation, InstallationId, InstallationRepositories};
use octocrab::Octocrab;
use serde::Serialize;

use crate::error::{AppError, AppResult};

const PER_PAGE: u8 = 100;

pub trait IRepo {
    fn owner(&self) -> &str;
    fn repo(&self) -> &str;
}

/// A bare `owner/repo` pair, for call sites that only have the two strings.
pub struct RepoRef<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
}

impl IRepo for RepoRef<'_> {
    fn owner(&self) -> &str {
        self.owner
    }
    fn repo(&self) -> &str {
        self.repo
    }
}

/// GitHub logins are case-insensitive, so the registry is keyed on a folded form.
fn account_key(login: &str) -> String {
    login.to_ascii_lowercase()
}

/// Credentials for authenticating as the GitHub App, read from the environment.
struct GitHubAppConfig {
    app_id: AppId,
    private_key: EncodingKey,
}

impl GitHubAppConfig {
    /// Returns `Ok(None)` when neither variable is set, so local development
    /// can run without GitHub credentials.
    fn from_env() -> AppResult<Option<Self>> {
        let app_id = std::env::var("GITHUB_APP_ID").ok();
        let private_key = std::env::var("GITHUB_APP_PRIVATE_KEY").ok();

        let (app_id, private_key) = match (app_id, private_key) {
            (None, None) => return Ok(None),
            (Some(app_id), Some(private_key)) => (app_id, private_key),
            _ => {
                return Err(AppError::Config(
                    "GITHUB_APP_ID and GITHUB_APP_PRIVATE_KEY must be set together".to_string(),
                ))
            }
        };

        let app_id = app_id
            .trim()
            .parse::<u64>()
            .map_err(|e| AppError::Config(format!("GITHUB_APP_ID is not a number: {}", e)))?;
        let private_key = EncodingKey::from_rsa_pem(private_key.as_bytes()).map_err(|e| {
            AppError::Config(format!(
                "GITHUB_APP_PRIVATE_KEY is not a valid RSA PEM: {}",
                e
            ))
        })?;

        Ok(Some(Self {
            app_id: AppId(app_id),
            private_key,
        }))
    }
}

/// A GitHub client authorized as one installation of the App.
///
/// Derefs to [`Octocrab`], so call sites read the same as any other client
/// (`crab.repos(..)`, `crab.ratelimit()`). Cloning shares the underlying
/// client and its cached installation token.
#[derive(Clone)]
pub struct InstallationClient {
    pub id: InstallationId,
    /// Login of the user or organization the App is installed on.
    pub account: String,
    crab: Arc<Octocrab>,
}

impl Deref for InstallationClient {
    type Target = Octocrab;

    fn deref(&self) -> &Octocrab {
        &self.crab
    }
}

#[derive(Serialize)]
struct PageParams {
    per_page: u8,
    page: u32,
}

impl InstallationClient {
    /// Every repository this installation can see, across all pages.
    pub async fn list_repos(&self) -> AppResult<Vec<octocrab::models::Repository>> {
        let mut all = Vec::new();
        let mut page: u32 = 1;
        loop {
            let params = PageParams {
                per_page: PER_PAGE,
                page,
            };
            let resp: InstallationRepositories = self
                .crab
                .get("/installation/repositories", Some(&params))
                .await?;
            let count = resp.repositories.len();
            all.extend(resp.repositories);
            if count < PER_PAGE as usize {
                break;
            }
            page += 1;
        }
        Ok(all)
    }
}

/// Registry of installation clients, keyed by the account each is installed on.
///
/// Cheap to clone: every clone shares the same clients and token caches.
#[derive(Clone)]
pub struct Octocrabs {
    inner: Arc<Inner>,
}

struct Inner {
    /// Authenticated as the App itself. `None` when no App is configured.
    app: Option<Octocrab>,
    /// Keyed by [`account_key`] of the installation's account login.
    installations: RwLock<HashMap<String, InstallationClient>>,
}

impl Octocrabs {
    /// A registry with no credentials; every lookup misses.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                app: None,
                installations: RwLock::new(HashMap::new()),
            }),
        }
    }

    fn for_app(config: GitHubAppConfig) -> AppResult<Self> {
        let app = Octocrab::builder()
            .app(config.app_id, config.private_key)
            .build()?;

        Ok(Self {
            inner: Arc::new(Inner {
                app: Some(app),
                installations: RwLock::new(HashMap::new()),
            }),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.app.is_some()
    }

    /// Snapshot of the known installations, ordered by account for stable output.
    pub fn installations(&self) -> Vec<InstallationClient> {
        let mut clients: Vec<InstallationClient> = match self.inner.installations.read() {
            Ok(map) => map.values().cloned().collect(),
            Err(e) => {
                log::error!("Installation registry lock poisoned: {}", e);
                Vec::new()
            }
        };
        clients.sort_by(|a, b| a.account.cmp(&b.account));
        clients
    }

    /// Re-list installations from GitHub.
    ///
    /// Existing clients are kept so their cached tokens stay valid; new
    /// installations are added and uninstalled ones are dropped.
    pub async fn refresh_installations(&self) -> AppResult<()> {
        let Some(app) = &self.inner.app else {
            return Ok(());
        };

        let mut found: Vec<Installation> = Vec::new();
        let mut page: u32 = 1;
        loop {
            let resp = app
                .apps()
                .installations()
                .per_page(PER_PAGE)
                .page(page)
                .send()
                .await?;
            let count = resp.items.len();
            found.extend(resp.items);
            if count < PER_PAGE as usize {
                break;
            }
            page += 1;
        }

        let mut next = HashMap::with_capacity(found.len());
        for installation in found {
            let key = account_key(&installation.account.login);
            let existing = self.get(&key).filter(|client| client.id == installation.id);
            let client = match existing {
                Some(client) => client,
                None => self.client_for(installation)?,
            };
            next.insert(key, client);
        }

        let mut map = self.inner.installations.write().map_err(|e| {
            AppError::Internal(format!("Installation registry lock poisoned: {}", e))
        })?;
        *map = next;

        Ok(())
    }

    /// The installation that can see `repo`, if any.
    ///
    /// Resolved by owner: an installation on an account covers that account's
    /// repositories. Unknown owners are looked up through the App and cached
    /// on success.
    pub async fn crab_for<T: IRepo>(&self, repo: &T) -> Option<InstallationClient> {
        let key = account_key(repo.owner());
        if let Some(client) = self.get(&key) {
            return Some(client);
        }

        let app = self.inner.app.as_ref()?;
        let installation = match app
            .apps()
            .get_repository_installation(repo.owner(), repo.repo())
            .await
        {
            Ok(installation) => installation,
            Err(e) => {
                log::debug!(
                    "No GitHub App installation can access {}/{}: {}",
                    repo.owner(),
                    repo.repo(),
                    e
                );
                return None;
            }
        };

        match self.client_for(installation) {
            Ok(client) => {
                self.insert(client.clone());
                Some(client)
            }
            Err(e) => {
                log::warn!(
                    "Failed to build client for installation covering {}/{}: {}",
                    repo.owner(),
                    repo.repo(),
                    e
                );
                None
            }
        }
    }

    fn client_for(&self, installation: Installation) -> AppResult<InstallationClient> {
        let Some(app) = &self.inner.app else {
            return Err(AppError::Config("GitHub App is not configured".to_string()));
        };

        let crab = app.installation(installation.id)?;
        log::info!(
            "GitHub App installation {} on {} ({} repositories)",
            installation.id,
            installation.account.login,
            installation
                .repository_selection
                .as_deref()
                .unwrap_or("unknown")
        );

        Ok(InstallationClient {
            id: installation.id,
            account: installation.account.login,
            crab: Arc::new(crab),
        })
    }

    fn get(&self, key: &str) -> Option<InstallationClient> {
        match self.inner.installations.read() {
            Ok(map) => map.get(key).cloned(),
            Err(e) => {
                log::error!("Installation registry lock poisoned: {}", e);
                None
            }
        }
    }

    fn insert(&self, client: InstallationClient) {
        match self.inner.installations.write() {
            Ok(mut map) => {
                map.insert(account_key(&client.account), client);
            }
            Err(e) => log::error!("Installation registry lock poisoned: {}", e),
        }
    }
}

/// Build the registry from `GITHUB_APP_ID` / `GITHUB_APP_PRIVATE_KEY`.
///
/// Missing configuration is not an error: the registry is empty and every
/// GitHub-backed feature degrades to a logged warning. Failing to list
/// installations at startup is also survivable — lookups fall through to
/// GitHub, and the next owner scan re-lists.
pub async fn initialize_octocrabs() -> AppResult<Octocrabs> {
    let Some(config) = GitHubAppConfig::from_env()? else {
        log::warn!("GITHUB_APP_ID / GITHUB_APP_PRIVATE_KEY not set; GitHub API access is disabled");
        return Ok(Octocrabs::disabled());
    };

    let octocrabs = Octocrabs::for_app(config)?;
    match octocrabs.refresh_installations().await {
        Ok(()) => log::info!(
            "GitHub App ready with {} installation(s)",
            octocrabs.installations().len()
        ),
        Err(e) => log::error!("Failed to list GitHub App installations: {}", e),
    }

    Ok(octocrabs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_key_folds_case() {
        assert_eq!(account_key("KJ800x"), "kj800x");
        assert_eq!(account_key("kj800x"), account_key("KJ800X"));
    }

    #[test]
    fn disabled_registry_has_no_installations() {
        let octocrabs = Octocrabs::disabled();
        assert!(!octocrabs.is_enabled());
        assert!(octocrabs.installations().is_empty());
    }

    #[tokio::test]
    async fn disabled_registry_never_resolves_a_repo() {
        let octocrabs = Octocrabs::disabled();
        let repo = RepoRef {
            owner: "kj800x",
            repo: "cicd",
        };
        assert!(octocrabs.crab_for(&repo).await.is_none());
    }
}
