use crate::{
    db::git_repo::GitRepo,
    error::AppResult,
    kubernetes::{deploy_handlers::DeployAction, DeployConfig},
};
use chrono::Utc;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

pub struct DeployEvent {
    pub name: String,
    pub timestamp: i64,
    pub initiator: String,
    pub config_sha: Option<String>,
    pub artifact_sha: Option<String>,
    pub artifact_branch: Option<String>,
    pub config_branch: Option<String>,
    pub prev_artifact_sha: Option<String>,
    pub prev_config_sha: Option<String>,
    pub artifact_repo_id: Option<i64>,
    pub config_repo_id: Option<i64>,
    pub config_version_hash: Option<String>,
    pub prev_config_version_hash: Option<String>,
}

impl DeployEvent {
    #[allow(unused)]
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(DeployEvent {
            name: row.get(0)?,
            timestamp: row.get(1)?,
            initiator: row.get(2)?,
            config_sha: row.get(3)?,
            artifact_sha: row.get(4)?,
            artifact_branch: row.get(5)?,
            config_branch: row.get(6)?,
            prev_artifact_sha: row.get(7)?,
            prev_config_sha: row.get(8)?,
            artifact_repo_id: row.get(9)?,
            config_repo_id: row.get(10)?,
            config_version_hash: row.get(11)?,
            prev_config_version_hash: row.get(12)?,
        })
    }

    pub fn from_user_deploy_action(
        action: &DeployAction,
        conn: &PooledConnection<SqliteConnectionManager>,
        config: &DeployConfig,
    ) -> AppResult<Option<Self>> {
        match action {
            DeployAction::Deploy {
                name,
                artifact,
                config: cfg_state,
            } => {
                let mut event = DeployEvent {
                    name: name.clone(),
                    timestamp: Utc::now().timestamp_millis(),
                    initiator: "USER".to_string(),
                    config_sha: Some(cfg_state.sha.clone()),
                    artifact_sha: artifact.as_ref().map(|a| a.sha.clone()),
                    artifact_branch: artifact.as_ref().and_then(|a| a.branch.clone()),
                    config_branch: cfg_state.branch.clone(),
                    prev_artifact_sha: None,
                    prev_config_sha: None,
                    artifact_repo_id: None,
                    config_repo_id: None,
                    config_version_hash: None,
                    prev_config_version_hash: None,
                };

                // Resolve repo ids from current DeployConfig
                let cfg_repo = config.config_repository();
                if let Ok(maybe_repo) = GitRepo::get_by_name(&cfg_repo.owner, &cfg_repo.repo, conn)
                {
                    if let Some(repo) = maybe_repo {
                        event.config_repo_id = Some(repo.id as i64);
                    }
                }
                if let Some(artifact_repo) = config.artifact_repository() {
                    if let Ok(maybe_repo) =
                        GitRepo::get_by_name(&artifact_repo.owner, &artifact_repo.repo, conn)
                    {
                        if let Some(repo) = maybe_repo {
                            event.artifact_repo_id = Some(repo.id as i64);
                        }
                    }
                }

                // Previous event snapshot for diffs
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT config_sha, artifact_sha, config_version_hash
                     FROM deploy_event
                     WHERE name = ?1
                     ORDER BY timestamp DESC
                     LIMIT 1",
                ) {
                    if let Ok(prev) = stmt
                        .query_row(params![event.name], |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        })
                        .optional()
                    {
                        if let Some((prev_cfg_sha, prev_art_sha, prev_cfg_hash)) = prev {
                            event.prev_config_sha = prev_cfg_sha;
                            event.prev_artifact_sha = prev_art_sha;
                            event.prev_config_version_hash = prev_cfg_hash;
                        }
                    }
                }

                // Current config version hash from deploy_config_version
                if let (Some(config_repo_id), Some(config_sha)) =
                    (&event.config_repo_id, &event.config_sha)
                {
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT hash FROM deploy_config_version
                         WHERE name = ?1 AND config_repo_id = ?2 AND config_commit_sha = ?3",
                    ) {
                        if let Ok(hash) = stmt
                            .query_row(params![event.name, config_repo_id, config_sha], |row| {
                                row.get::<_, String>(0)
                            })
                            .optional()
                        {
                            event.config_version_hash = hash;
                        }
                    }
                }

                Ok(Some(event))
            }
            DeployAction::Undeploy { name } => {
                let mut event = DeployEvent {
                    name: name.clone(),
                    timestamp: Utc::now().timestamp_millis(),
                    initiator: "USER".to_string(),
                    config_sha: None,
                    artifact_sha: None,
                    artifact_branch: None,
                    config_branch: None,
                    prev_artifact_sha: None,
                    prev_config_sha: None,
                    artifact_repo_id: None,
                    config_repo_id: None,
                    config_version_hash: None,
                    prev_config_version_hash: None,
                };
                // Resolve repo ids for consistency
                let cfg_repo = config.config_repository();
                if let Ok(maybe_repo) = GitRepo::get_by_name(&cfg_repo.owner, &cfg_repo.repo, conn)
                {
                    if let Some(repo) = maybe_repo {
                        event.config_repo_id = Some(repo.id as i64);
                    }
                }
                if let Some(artifact_repo) = config.artifact_repository() {
                    if let Ok(maybe_repo) =
                        GitRepo::get_by_name(&artifact_repo.owner, &artifact_repo.repo, conn)
                    {
                        if let Some(repo) = maybe_repo {
                            event.artifact_repo_id = Some(repo.id as i64);
                        }
                    }
                }
                Ok(Some(event))
            }
            DeployAction::ToggleAutodeploy { .. } => {
                // No event
                Ok(None)
            }
        }
    }

    pub fn insert(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Self> {
        conn.prepare("INSERT INTO deploy_event (name, timestamp, initiator, config_sha, artifact_sha, artifact_branch, config_branch, prev_artifact_sha, prev_config_sha, artifact_repo_id, config_repo_id, config_version_hash, prev_config_version_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)")?
          .execute(params![
            self.name,
            self.timestamp,
            self.initiator,
            self.config_sha,
            self.artifact_sha,
            self.artifact_branch,
            self.config_branch,
            self.prev_artifact_sha,
            self.prev_config_sha,
            self.artifact_repo_id,
            self.config_repo_id,
            self.config_version_hash,
            self.prev_config_version_hash
          ])?;

        Ok(Self {
            name: self.name.clone(),
            timestamp: self.timestamp,
            initiator: self.initiator.clone(),
            config_sha: self.config_sha.clone(),
            artifact_sha: self.artifact_sha.clone(),
            artifact_branch: self.artifact_branch.clone(),
            config_branch: self.config_branch.clone(),
            prev_artifact_sha: self.prev_artifact_sha.clone(),
            prev_config_sha: self.prev_config_sha.clone(),
            artifact_repo_id: self.artifact_repo_id,
            config_repo_id: self.config_repo_id,
            config_version_hash: self.config_version_hash.clone(),
            prev_config_version_hash: self.prev_config_version_hash.clone(),
        })
    }

    #[allow(unused)]
    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE deploy_event SET timestamp = ?2, initiator = ?3, config_sha = ?4, artifact_sha = ?5, artifact_branch = ?6, config_branch = ?7, prev_artifact_sha = ?8, prev_config_sha = ?9, artifact_repo_id = ?10, config_repo_id = ?11, config_version_hash = ?12, prev_config_version_hash = ?13 WHERE name = ?1")?
          .execute(params![
            self.name,
            self.timestamp,
            self.initiator,
            self.config_sha,
            self.artifact_sha,
            self.artifact_branch,
            self.config_branch,
            self.prev_artifact_sha,
            self.prev_config_sha,
            self.artifact_repo_id,
            self.config_repo_id,
            self.config_version_hash,
            self.prev_config_version_hash
          ])?;

        Ok(())
    }
}
