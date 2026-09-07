//! Autodeploy: "deploy latest", run automatically when a build succeeds on
//! a branch some config is tracking.
//!
//! The gates are the ones the design lays out. A config takes part only if
//! its autodeploy flag is on and it is not orphaned. Its `SHA` parameter
//! must be tracking a branch (the default or an override) that contains the
//! commit; a pinned parameter never moves. A temporary deployment is left
//! alone, since someone is babysitting it, and so is a config with an
//! active blocker (the deploy runner refuses those anyway). Finally the
//! deploy only runs if "latest" would actually change what is deployed, so
//! the several check runs a commit produces do not each record a revision.

use anyhow::Context;
use kube::{Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serenity::async_trait;

use crate::{
    build_status::BuildStatus,
    crab_ext::Octocrabs,
    db::{blocker::Blocker, git_commit::GitCommit, git_repo::GitRepo},
    deploys::{run_action, SelectionIntent},
    kubernetes::{
        api::get_all_deploy_configs, parameters::SHA_PARAMETER, repo::DeploymentState,
        selections::Mode, DeployConfig,
    },
    web::Action,
    webhooks::{models::CheckRunEvent, WebhookHandler},
};

pub struct AutodeployHandler {
    pool: Pool<SqliteConnectionManager>,
    client: Client,
    octocrabs: Octocrabs,
}

impl AutodeployHandler {
    pub fn new(pool: Pool<SqliteConnectionManager>, client: Client, octocrabs: Octocrabs) -> Self {
        Self {
            pool,
            client,
            octocrabs,
        }
    }
}

/// Why a config did not autodeploy, for the log.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    Off,
    Orphaned,
    NoArtifact,
    OtherRepo,
    Pinned,
    NotOnBranch(String),
    Temporary,
    Blocked,
    AlreadyLatest,
}

/// The pure part of the decision: everything except database and cluster
/// lookups. Returns the branch the config is tracking if it should deploy.
pub fn tracked_branch_for(
    config: &DeployConfig,
    owner: &str,
    repo: &str,
    branches_with_commit: &[String],
) -> Result<String, Skip> {
    if !config.autodeploy() {
        return Err(Skip::Off);
    }
    if config.is_orphaned() {
        return Err(Skip::Orphaned);
    }
    let artifact = config.artifact_repository().ok_or(Skip::NoArtifact)?;
    if !artifact.owner.eq_ignore_ascii_case(owner) || !artifact.repo.eq_ignore_ascii_case(repo) {
        return Err(Skip::OtherRepo);
    }
    let selection = config.selection(SHA_PARAMETER);
    let channel = match selection.mode() {
        Mode::Pin(_) => return Err(Skip::Pinned),
        Mode::Track(branch) => branch.to_string(),
        Mode::Default => artifact.branch.clone(),
    };
    if !branches_with_commit.iter().any(|b| b == &channel) {
        return Err(Skip::NotOnBranch(channel));
    }
    if config.is_temporary_deployment() {
        return Err(Skip::Temporary);
    }
    Ok(channel)
}

#[async_trait]
impl WebhookHandler for AutodeployHandler {
    async fn handle_check_run(&self, payload: CheckRunEvent) -> Result<(), anyhow::Error> {
        let run = &payload.check_run;
        if BuildStatus::of(&run.status, &run.conclusion.as_deref()) != BuildStatus::Success {
            return Ok(());
        }
        let owner = payload.repository.owner.login.clone();
        let repo_name = payload.repository.name.clone();
        let sha = run.check_suite.head_sha.clone();

        let conn = self
            .pool
            .get()
            .context("Failed to get database connection")?;
        let Some(repo) = GitRepo::get_by_name(&owner, &repo_name, &conn)? else {
            return Ok(());
        };
        let Some(commit) = GitCommit::get_by_sha(&sha, repo.id, &conn)? else {
            return Ok(());
        };
        let branches: Vec<String> = commit
            .get_branches(&conn)?
            .into_iter()
            .map(|b| b.name)
            .collect();
        if branches.is_empty() {
            return Ok(());
        }

        let configs = get_all_deploy_configs(&self.client).await?;
        for config in configs {
            let name = config.name_any();
            let branch = match tracked_branch_for(&config, &owner, &repo_name, &branches) {
                Ok(branch) => branch,
                Err(Skip::Off) | Err(Skip::NoArtifact) | Err(Skip::OtherRepo) => continue,
                Err(skip) => {
                    log::info!("Autodeploy: skipping {} ({:?})", name, skip);
                    continue;
                }
            };
            if !Blocker::active_for(&conn, &name)?.is_empty() {
                log::info!("Autodeploy: skipping {} ({:?})", name, Skip::Blocked);
                continue;
            }
            // Only deploy if latest would change something; a commit's
            // several check runs must not each produce a revision.
            let wanted = match DeploymentState::from_action(&Action::DeployLatest, &config, &conn) {
                Ok(state) => state,
                Err(e) => {
                    log::warn!("Autodeploy: could not resolve latest for {}: {}", name, e);
                    continue;
                }
            };
            if wanted == config.deployment_state() {
                log::debug!(
                    "Autodeploy: skipping {} ({:?}, already at latest of {})",
                    name,
                    Skip::AlreadyLatest,
                    branch
                );
                continue;
            }
            log::info!(
                "Autodeploy: {} tracks {} of {}/{}; deploying after build of {}",
                name,
                branch,
                owner,
                repo_name,
                &sha[..sha.len().min(12)]
            );
            let result = run_action(
                &Action::DeployLatest,
                &config,
                &self.client,
                &self.octocrabs,
                &self.pool,
                "autodeploy",
                &SelectionIntent::default(),
            )
            .await;
            crate::metrics::get().deploy_actions.add(
                1,
                &[
                    opentelemetry::KeyValue::new("name", name.clone()),
                    opentelemetry::KeyValue::new("action", "autodeploy"),
                    opentelemetry::KeyValue::new(
                        "result",
                        if result.is_ok() { "success" } else { "error" },
                    ),
                ],
            );
            if let Err(e) = result {
                log::error!("Autodeploy of {} failed: {}", name, e);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::deploy_config::{
        DeployConfigSpec, DeployConfigSpecFields, DeployConfigStatus,
    };
    use crate::kubernetes::parameters::{ParameterSource, ParameterValue};
    use crate::kubernetes::repo::{RepositoryBranch, ShaMaybeBranch};
    use crate::kubernetes::selections::{Durability, Selection};
    use crate::kubernetes::Repository;

    fn config(autodeploy: bool, deployed_branch: Option<&str>) -> DeployConfig {
        let mut dc = DeployConfig::new(
            "site",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters: ParameterSource::sha_map(Some(RepositoryBranch {
                        owner: "kj800x".into(),
                        repo: "site".into(),
                        branch: "master".into(),
                    })),
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "kj800x".into(),
                        repo: "site".into(),
                    },
                    specs: vec![],
                },
            },
        );
        let mut status = DeployConfigStatus {
            autodeploy: Some(autodeploy),
            config: Some(ShaMaybeBranch {
                sha: "cfg".into(),
                branch: Some("master".into()),
            }),
            ..Default::default()
        };
        status.parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::from(ShaMaybeBranch {
                sha: "old".into(),
                branch: deployed_branch.map(String::from),
            }),
        );
        dc.status = Some(status);
        dc
    }

    #[test]
    fn decides_per_the_design_gates() {
        let on_master = vec!["master".to_string()];
        assert_eq!(
            tracked_branch_for(&config(true, Some("master")), "kj800x", "site", &on_master),
            Ok("master".into())
        );
        assert_eq!(
            tracked_branch_for(&config(true, Some("master")), "KJ800X", "Site", &on_master),
            Ok("master".into()),
            "repo match is case-insensitive"
        );
        assert_eq!(
            tracked_branch_for(&config(false, Some("master")), "kj800x", "site", &on_master),
            Err(Skip::Off)
        );
        assert_eq!(
            tracked_branch_for(&config(true, Some("master")), "kj800x", "other", &on_master),
            Err(Skip::OtherRepo)
        );
        assert_eq!(
            tracked_branch_for(&config(true, None), "kj800x", "site", &on_master),
            Err(Skip::Pinned),
            "a derived pin never moves"
        );
        assert_eq!(
            tracked_branch_for(
                &config(true, Some("master")),
                "kj800x",
                "site",
                &["feature".to_string()]
            ),
            Err(Skip::NotOnBranch("master".into()))
        );
        // A derived branch override is temporary, so it is left alone even
        // though the commit is on its branch.
        assert_eq!(
            tracked_branch_for(
                &config(true, Some("feature")),
                "kj800x",
                "site",
                &["feature".to_string()]
            ),
            Err(Skip::Temporary)
        );
        // A standing override on a branch does autodeploy from that branch.
        let mut standing = config(true, Some("feature"));
        standing.spec.spec.selections.insert(
            SHA_PARAMETER.into(),
            Selection::track("feature", Durability::Standing),
        );
        assert_eq!(
            tracked_branch_for(&standing, "kj800x", "site", &["feature".to_string()]),
            Ok("feature".into())
        );
    }
}
