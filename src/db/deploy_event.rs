use crate::error::AppResult;
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

    pub fn insert(
        event: &DeployEvent,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT INTO deploy_event (name, timestamp, initiator, config_sha, artifact_sha, artifact_branch) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
          .execute(params![event.name, event.timestamp, event.initiator, event.config_sha, event.artifact_sha, event.artifact_branch])?;

        Ok(Self {
            name: event.name.clone(),
            timestamp: event.timestamp,
            initiator: event.initiator.clone(),
            config_sha: event.config_sha.clone(),
            artifact_sha: event.artifact_sha.clone(),
            artifact_branch: event.artifact_branch.clone(),
        })
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE deploy_event SET timestamp = ?2, initiator = ?3, config_sha = ?4, artifact_sha = ?5, artifact_branch = ?6 WHERE name = ?1")?
          .execute(params![self.name, self.timestamp, self.initiator, self.config_sha, self.artifact_sha, self.artifact_branch])?;

        Ok(())
    }
}
