use std::collections::{BTreeMap, HashMap};

use kube::{api::ObjectMeta, Client, ResourceExt};
use octocrab::models::repos::Content;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serenity::async_trait;

use crate::{
    crab_ext::{IRepo, Octocrabs},
    db::{
        deploy_config::DeployConfig as DbDeployConfig, deploy_config_version::DeployConfigVersion,
        git_repo::GitRepo,
    },
    error::{AppError, AppResult},
    kubernetes::{
        deploy_config::{
            DeployChecks, DeployConfig, DeployConfigSpec, DeployConfigSpecFields, Template,
        },
        parameters::{ParameterSource, ParameterSources, SHA_PARAMETER},
        repo::RepositoryBranch,
        test_mode,
        webhook_handlers::{
            orphan_deploy_configs_for_repo, update_deploy_configs_by_defining_repo,
        },
        Repository,
    },
    webhooks::{
        models::{InstallationRepositoriesEvent, PushEvent, RepositoryEvent},
        util::extract_branch_name,
        WebhookHandler,
    },
};

pub struct ConfigSyncHandler {
    pool: Pool<SqliteConnectionManager>,
    client: Client,
    octocrabs: Octocrabs,
}

impl ConfigSyncHandler {
    pub fn new(pool: Pool<SqliteConnectionManager>, client: Client, octocrabs: Octocrabs) -> Self {
        Self {
            pool,
            client,
            octocrabs,
        }
    }

    /// A repository we can no longer read from or build: deleted, archived,
    /// or removed from the App installation. Any deploy config that uses it
    /// as either its config repo or its artifact repo is orphaned, and the
    /// matching database rows are retired.
    async fn handle_repo_gone(
        &self,
        owner: &str,
        name: &str,
        repo_id: u64,
        reason: &str,
    ) -> Result<(), anyhow::Error> {
        {
            let conn = self.pool.get()?;
            for db_config in DbDeployConfig::get_by_any_repo_id(repo_id, &conn)? {
                if db_config.active {
                    DbDeployConfig::mark_inactive(&db_config.name, &conn)?;
                }
            }
        }

        let repository = Repository {
            owner: owner.to_string(),
            repo: name.to_string(),
        };
        let affected = orphan_deploy_configs_for_repo(&self.client, &repository, reason).await?;

        if affected.is_empty() {
            log::debug!(
                "Repository {}/{} {}, no deploy configs reference it",
                owner,
                name,
                reason
            );
        } else {
            log::info!(
                "Repository {}/{} {}, orphaned {} deploy config(s): {}",
                owner,
                name,
                reason,
                affected.len(),
                affected.join(", ")
            );
        }

        Ok(())
    }
}

/// Sync deploy configs for a repository at a specific commit SHA.
/// This is idempotent - calling multiple times with the same SHA won't create duplicates.
pub async fn sync_deploy_configs_for_commit(
    octocrabs: &Octocrabs,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    repo_owner: &str,
    repo_name: &str,
    repo_id: u64,
    commit_sha: &str,
) -> Result<(), anyhow::Error> {
    let repository = Repository {
        owner: repo_owner.to_string(),
        repo: repo_name.to_string(),
    };

    let deploy_configs =
        fetch_deploy_configs_by_sha(octocrabs, repository.clone(), commit_sha).await?;
    // Deploys and previews of this commit read it from the memo.
    remember_deploy_configs(&repository, commit_sha, &deploy_configs);

    // Get connection after async work is done
    let conn = pool.get()?;

    let existing_deploy_configs = DbDeployConfig::get_by_config_repo_id(repo_id, &conn)?;

    let current_deploy_config_names = deploy_configs
        .iter()
        .map(|dc| dc.name_any())
        .collect::<Vec<String>>();

    for deploy_config in deploy_configs.clone() {
        let artifact_repo_id = match deploy_config.artifact_repository() {
            Some(repository) => {
                let repo = GitRepo::get_by_name(&repository.owner, &repository.repo, &conn)?
                    .ok_or_else(|| AppError::NotFound(format!(
                        "Artifact repository {}/{} not found in database. Use the bootstrap feature to sync this repository first.",
                        repository.owner, repository.repo
                    )))?;
                Some(repo.id)
            }
            None => None,
        };

        let db_config = DbDeployConfig {
            name: deploy_config.name_any(),
            team: deploy_config.team().to_string(),
            kind: deploy_config.kind().to_string(),
            config_repo_id: repo_id,
            artifact_repo_id,
            active: true,
        };

        DbDeployConfig::upsert(&db_config, &conn)?;

        DeployConfigVersion::upsert(
            &DeployConfigVersion {
                name: deploy_config.name_any(),
                config_repo_id: repo_id,
                config_commit_sha: commit_sha.to_string(),
                hash: deploy_config.spec_hash(),
            },
            &conn,
        )?;
    }

    let deleted_deploy_config_names = existing_deploy_configs
        .iter()
        .filter(|dc| !current_deploy_config_names.contains(&dc.name))
        .map(|dc| dc.name.clone())
        .collect::<Vec<String>>();

    for deleted_deploy_config_name in &deleted_deploy_config_names {
        DbDeployConfig::mark_inactive(deleted_deploy_config_name, &conn)?;
    }

    // Drop connection before async work
    drop(conn);

    update_deploy_configs_by_defining_repo(
        client,
        &deploy_configs,
        &deleted_deploy_config_names,
        &repository,
    )
    .await?;

    // Watchtower follows the tag parameters declared across all configs.
    // Advisory: the startup and periodic passes retry, and a sync must not
    // fail because the tag feed is down.
    if let Err(e) =
        crate::watchtower::reconcile_registrations(crate::watchtower::Watchtower::global(), client)
            .await
    {
        log::warn!("watchtower registration reconcile after sync failed: {}", e);
    }

    Ok(())
}

#[async_trait]
impl WebhookHandler for ConfigSyncHandler {
    async fn handle_push(&self, event: PushEvent) -> Result<(), anyhow::Error> {
        log::debug!("Received push event:\n{:#?}", event);

        // Push to default branch, so we need to sync the deploy configs
        if extract_branch_name(&event.r#ref) == Some(event.repository.default_branch.clone()) {
            #[allow(clippy::expect_used)]
            let config_commit_sha = event
                .head_commit
                .as_ref()
                .expect("Head commit should be present")
                .id
                .clone();

            sync_deploy_configs_for_commit(
                &self.octocrabs,
                &self.client,
                &self.pool,
                &event.repository.owner.login,
                &event.repository.name,
                event.repository.id,
                &config_commit_sha,
            )
            .await?;
        }

        Ok(())
    }

    async fn handle_repository(&self, event: RepositoryEvent) -> Result<(), anyhow::Error> {
        // `unarchived` is deliberately not handled here: clearing the orphan
        // flag happens on the next push to the config repo's default branch,
        // which re-syncs the config and resets `status.orphaned`.
        match event.action.as_str() {
            "deleted" | "archived" => {
                self.handle_repo_gone(
                    &event.repository.owner.login,
                    &event.repository.name,
                    event.repository.id,
                    &event.action,
                )
                .await
            }
            _ => Ok(()),
        }
    }

    async fn handle_installation_repositories(
        &self,
        event: InstallationRepositoriesEvent,
    ) -> Result<(), anyhow::Error> {
        if event.action != "removed" {
            return Ok(());
        }

        // Best-effort per repo so one failure doesn't hide the others.
        let mut first_error = None;
        for removed in &event.repositories_removed {
            let Some((owner, name)) = removed.full_name.split_once('/') else {
                log::warn!("Malformed repository full_name: {}", removed.full_name);
                continue;
            };

            if let Err(e) = self
                .handle_repo_gone(owner, name, removed.id, "removed from the installation")
                .await
            {
                log::error!(
                    "Failed to orphan deploy configs for {}: {:#}",
                    removed.full_name,
                    e
                );
                first_error.get_or_insert(e);
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

// MARK: Maybe move everything below this to a separate file?

fn default_branch() -> String {
    "master".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GitHubArtifactRepo {
    owner: String,
    repo: String,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GitHubDeployConfig {
    /// Sugar for a commit parameter named `SHA`.
    #[serde(rename = "artifactRepo")]
    artifact_repo: Option<GitHubArtifactRepo>,
    /// Declared parameters: `{type: commit, owner, repo, branch}` or
    /// `{type: value, default}`, keyed by the `$NAME` used in manifests.
    #[serde(default)]
    parameters: BTreeMap<String, ParameterSource>,
    team: String,
    kind: String,
    namespace: String,
    /// What happens around a deploy: `onFailure` and `rolloutDeadline`.
    #[serde(default)]
    checks: DeployChecks,
}

impl GitHubDeployConfig {
    /// Every declared parameter, with `artifactRepo` folded in as `SHA`.
    /// Declaring `SHA` both ways is a mistake worth refusing.
    fn parameter_sources(&self) -> AppResult<ParameterSources> {
        let mut sources = self.parameters.clone();
        if let Some(artifact) = &self.artifact_repo {
            if sources.contains_key(SHA_PARAMETER) {
                return Err(AppError::InvalidInput(format!(
                    "both artifactRepo and parameters.{SHA_PARAMETER} are set; declare one"
                )));
            }
            sources.insert(
                SHA_PARAMETER.to_string(),
                ParameterSource::from(RepositoryBranch {
                    owner: artifact.owner.clone(),
                    repo: artifact.repo.clone(),
                    branch: artifact.branch.clone(),
                }),
            );
        }
        Ok(sources)
    }
}

/// Whether a GitHub call failed because the path does not exist at the ref.
fn is_not_found(e: &octocrab::Error) -> bool {
    matches!(e, octocrab::Error::GitHub { source, .. } if source.status_code.as_u16() == 404)
}

/// The decoded text of one file at a commit.
async fn read_file(
    crab: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    sha: &str,
    path: &str,
) -> AppResult<String> {
    let Ok(mut content) = crab
        .repos(owner, repo)
        .get_content()
        .r#ref(sha)
        .path(path)
        .send()
        .await
    else {
        log::error!("Failed to read {}", path);
        return Err(AppError::NotFound(format!("Failed to read {}", path)));
    };
    let items = content.take_items();
    let Some(text) = items.first().and_then(|c| c.decoded_content()) else {
        log::error!("Failed to decode {}", path);
        return Err(AppError::Internal(format!("Failed to decode {}", path)));
    };
    if text.is_empty() {
        log::warn!("  WARNING: File {} is empty!", path);
    }
    Ok(text)
}

/// The files of `.deploy/<name>/`, read in parallel, in listing order.
async fn read_config_dir(
    crab: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    sha: &str,
    config_name: &str,
    files: Vec<Content>,
) -> AppResult<Vec<(String, String)>> {
    let reads = files.iter().map(|file| {
        let path = format!(".deploy/{}/{}", config_name, file.name);
        async move { read_file(crab, owner, repo, sha, &path).await }
    });
    let texts = futures_util::future::join_all(reads).await;
    files
        .into_iter()
        .zip(texts)
        .map(|(file, text)| Ok((file.name, text?)))
        .collect()
}

/// A DeployConfig from its `.deploy/<name>.yaml` and the files beside it.
fn build_deploy_config(
    owner: &str,
    repo: &str,
    config_name: &str,
    config_content: &str,
    child_files: Vec<(String, String)>,
) -> AppResult<DeployConfig> {
    let config: GitHubDeployConfig =
        serde_yaml::from_str(config_content).map_err(AppError::Yaml)?;
    let child_files: Vec<Value> = child_files
        .into_iter()
        .map(|(file_name, file)| {
            let parsed: Value = serde_yaml::from_str(&file).map_err(AppError::Yaml)?;
            if parsed.is_null() {
                log::warn!(
                    "  WARNING: Child file {} parsed as null! Content was: {}",
                    file_name,
                    file
                );
            }
            Ok(Template::stored(&file_name, parsed))
        })
        .collect::<Result<Vec<Value>, AppError>>()?;
    let parameters = config.parameter_sources()?;
    Ok(DeployConfig {
        spec: DeployConfigSpec {
            spec: DeployConfigSpecFields {
                checks: config.checks.clone(),
                parameters,
                // Selections are user intent, owned by the deploy handler;
                // empty here means "leave whatever is on the object".
                selections: Default::default(),
                patches: vec![],
                config: Repository {
                    owner: owner.to_owned(),
                    repo: repo.to_owned(),
                },
                kind: config.kind,
                specs: child_files,
                team: config.team.clone(),
            },
        },
        metadata: ObjectMeta {
            name: Some(config_name.to_owned()),
            namespace: Some(test_mode::deploy_namespace(&config.namespace)),
            ..ObjectMeta::default()
        },
        status: None,
    })
}

/// One config's definition at a commit, reading only its own files:
/// `.deploy/<name>.yaml` and `.deploy/<name>/`, a handful of GitHub calls
/// instead of the whole tree. `None` when the commit does not define it.
pub async fn fetch_deploy_config_by_sha(
    octocrabs: &Octocrabs,
    repository: impl IRepo,
    sha: &str,
    config_name: &str,
) -> AppResult<Option<DeployConfig>> {
    let Some(crab) = octocrabs.crab_for(&repository).await else {
        return Err(AppError::NotFound(
            "No octocrab found for this repo".to_owned(),
        ));
    };
    let owner = repository.owner().to_string();
    let repo = repository.repo().to_string();
    let yaml_path = format!(".deploy/{}.yaml", config_name);
    let dir_path = format!(".deploy/{}", config_name);
    let get = |path: String| {
        let crab = crab.clone();
        let (owner, repo) = (owner.clone(), repo.clone());
        async move {
            crab.repos(&owner, &repo)
                .get_content()
                .r#ref(sha)
                .path(path)
                .send()
                .await
        }
    };
    let (yaml, dir) =
        futures_util::future::join(get(yaml_path.clone()), get(dir_path.clone())).await;
    let config_content = match yaml {
        Ok(mut content) => match content
            .take_items()
            .first()
            .and_then(|c| c.decoded_content())
        {
            Some(text) => text,
            None => {
                return Err(AppError::Internal(format!(
                    "Failed to decode {}",
                    yaml_path
                )));
            }
        },
        Err(e) if is_not_found(&e) => {
            log::warn!(
                "{} is not defined at {}/{}@{}",
                config_name,
                owner,
                repo,
                sha
            );
            return Ok(None);
        }
        Err(e) => {
            log::error!("Failed to read {}: {:?}", yaml_path, e);
            return Err(AppError::NotFound(format!("Failed to read {}", yaml_path)));
        }
    };
    let files = match dir {
        Ok(content) => content.items,
        Err(e) if is_not_found(&e) => vec![],
        Err(e) => {
            log::error!("Failed to read {}: {:?}", dir_path, e);
            return Err(AppError::NotFound(format!("Failed to read {}", dir_path)));
        }
    };
    let child_files = read_config_dir(&crab, &owner, &repo, sha, config_name, files).await?;
    build_deploy_config(&owner, &repo, config_name, &config_content, child_files).map(Some)
}

/// Process-wide memos of what `.deploy/` held at a commit: the content at
/// a full sha never changes, so one GitHub fetch per (repo, sha) serves
/// every preview poll and deploy after it. Config sync fills the whole-tree
/// memo as it reads each pushed commit; a deploy that finds nothing there
/// reads just its own config into the per-config memo. Only settled
/// answers are kept (a fetch that failed, or found no configs, is retried
/// next time), and each map is emptied when it grows past a few hundred
/// entries.
type CommitKey = (String, String, String);
type ConfigsAtCommit = std::sync::Mutex<HashMap<CommitKey, Vec<DeployConfig>>>;
static CONFIGS_AT_COMMIT: std::sync::OnceLock<ConfigsAtCommit> = std::sync::OnceLock::new();
type ConfigKey = (String, String, String, String);
type ConfigAtCommit = std::sync::Mutex<HashMap<ConfigKey, DeployConfig>>;
static CONFIG_AT_COMMIT: std::sync::OnceLock<ConfigAtCommit> = std::sync::OnceLock::new();

const CONFIG_CACHE_LIMIT: usize = 256;

fn cache_key(repository: &impl IRepo, sha: &str) -> CommitKey {
    (
        repository.owner().to_ascii_lowercase(),
        repository.repo().to_ascii_lowercase(),
        sha.to_ascii_lowercase(),
    )
}

/// Only a full sha names one immutable tree.
fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn memo_insert<K: std::hash::Hash + Eq, V>(
    memo: &std::sync::OnceLock<std::sync::Mutex<HashMap<K, V>>>,
    key: K,
    value: V,
) {
    if let Ok(mut c) = memo.get_or_init(Default::default).lock() {
        if c.len() >= CONFIG_CACHE_LIMIT {
            c.clear();
        }
        c.insert(key, value);
    }
}

/// Record a whole-tree read made elsewhere (config sync), so the previews
/// and deploys of that commit need no GitHub calls of their own.
pub fn remember_deploy_configs(repository: &impl IRepo, sha: &str, configs: &[DeployConfig]) {
    if is_full_sha(sha) && !configs.is_empty() {
        memo_insert(
            &CONFIGS_AT_COMMIT,
            cache_key(repository, sha),
            configs.to_vec(),
        );
    }
}

/// One config's definition at a commit, through the memos: the whole-tree
/// read config sync made of that commit, else this config's own files.
pub async fn fetch_deploy_config_cached(
    octocrabs: &Octocrabs,
    repository: impl IRepo,
    sha: &str,
    config_name: &str,
) -> AppResult<Option<DeployConfig>> {
    if !is_full_sha(sha) {
        return fetch_deploy_config_by_sha(octocrabs, repository, sha, config_name).await;
    }
    let (owner, repo, sha_key) = cache_key(&repository, sha);
    let config_key = (owner, repo, sha_key, config_name.to_owned());
    if let Some(found) = CONFIGS_AT_COMMIT
        .get_or_init(Default::default)
        .lock()
        .ok()
        .and_then(|c| {
            c.get(&cache_key(&repository, sha)).map(|configs| {
                configs
                    .iter()
                    .find(|c| c.name_any() == config_name)
                    .cloned()
            })
        })
    {
        return Ok(found);
    }
    if let Some(found) = CONFIG_AT_COMMIT
        .get_or_init(Default::default)
        .lock()
        .ok()
        .and_then(|c| c.get(&config_key).cloned())
    {
        return Ok(Some(found));
    }
    let config = fetch_deploy_config_by_sha(octocrabs, repository, sha, config_name).await?;
    if let Some(config) = &config {
        memo_insert(&CONFIG_AT_COMMIT, config_key, config.clone());
    }
    Ok(config)
}

pub async fn fetch_deploy_configs_by_sha(
    octocrabs: &Octocrabs,
    repository: impl IRepo,
    sha: &str,
) -> AppResult<Vec<DeployConfig>> {
    let Some(crab) = octocrabs.crab_for(&repository).await else {
        log::error!(
            "No octocrab found for repo {}/{}",
            repository.owner(),
            repository.repo()
        );
        return Err(AppError::NotFound(
            "No octocrab found for this repo".to_owned(),
        ));
    };

    let owner = repository.owner().to_string();
    let repo = repository.repo().to_string();

    let content = match crab
        .repos(&owner, &repo)
        .get_content()
        .r#ref(sha)
        .path(".deploy")
        .send()
        .await
    {
        Ok(content) => content,
        Err(e) => {
            log::warn!("Failed to fetch .deploy directory: {:?}", e);
            // FIXME: Should actually confirm that this is a 404 before saying Ok(())
            return Ok(vec![]);
        }
    };

    let entries: HashMap<String, Content> = content
        .items
        .into_iter()
        .map(|item| (item.name.clone(), item))
        .collect();
    let configs: Vec<&str> = entries
        .keys()
        .filter_map(|name| name.strip_suffix(".yaml"))
        .collect();

    let mut final_deploy_configs: Vec<DeployConfig> = vec![];

    for config_name in configs {
        let child_files = if entries.contains_key(config_name) {
            let Ok(content_items) = crab
                .repos(&owner, &repo)
                .get_content()
                .r#ref(sha)
                .path(format!(".deploy/{}", config_name))
                .send()
                .await
            else {
                log::error!("Failed to read .deploy/{}", config_name);
                return Err(AppError::NotFound(format!(
                    "Failed to read .deploy/{}",
                    config_name
                )));
            };
            read_config_dir(&crab, &owner, &repo, sha, config_name, content_items.items).await?
        } else {
            vec![]
        };
        let config_content = read_file(
            &crab,
            &owner,
            &repo,
            sha,
            &format!(".deploy/{}.yaml", config_name),
        )
        .await?;
        final_deploy_configs.push(build_deploy_config(
            &owner,
            &repo,
            config_name,
            &config_content,
            child_files,
        )?);
    }

    Ok(final_deploy_configs)
}
