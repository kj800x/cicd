use crate::{error::AppResult, kubernetes::deploy_handlers::DeployAction};
use chrono::Utc;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct DeployEvent {
    pub name: String,
    pub timestamp: i64,
    pub initiator: String,
    pub config_sha: Option<String>,
    pub artifact_sha: Option<String>,
    pub artifact_branch: Option<String>,
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
        })
    }

    pub fn from_user_deploy_action(action: &DeployAction) -> AppResult<Option<Self>> {
        match action {
            DeployAction::Deploy {
                name,
                artifact,
                config,
            } => Ok(Some(DeployEvent {
                name: name.clone(),
                timestamp: Utc::now().timestamp_millis(),
                initiator: "USER".to_string(),
                config_sha: Some(config.sha.clone()),
                artifact_sha: artifact.as_ref().map(|a| a.sha.clone()),
                artifact_branch: artifact.as_ref().and_then(|a| a.branch.clone()),
            })),
            DeployAction::Undeploy { name } => Ok(Some(DeployEvent {
                name: name.clone(),
                timestamp: Utc::now().timestamp_millis(),
                initiator: "USER".to_string(),
                config_sha: None,
                artifact_sha: None,
                artifact_branch: None,
            })),
            DeployAction::ToggleAutodeploy { .. } => {
                // No event
                Ok(None)
            }
        }
    }

    pub fn insert(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Self> {
        conn.prepare("INSERT INTO deploy_event (name, timestamp, initiator, config_sha, artifact_sha, artifact_branch) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
          .execute(params![self.name, self.timestamp, self.initiator, self.config_sha, self.artifact_sha, self.artifact_branch])?;

        Ok(Self {
            name: self.name.clone(),
            timestamp: self.timestamp,
            initiator: self.initiator.clone(),
            config_sha: self.config_sha.clone(),
            artifact_sha: self.artifact_sha.clone(),
            artifact_branch: self.artifact_branch.clone(),
        })
    }

    #[allow(unused)]
    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE deploy_event SET timestamp = ?2, initiator = ?3, config_sha = ?4, artifact_sha = ?5, artifact_branch = ?6 WHERE name = ?1")?
          .execute(params![self.name, self.timestamp, self.initiator, self.config_sha, self.artifact_sha, self.artifact_branch])?;

        Ok(())
    }
}
