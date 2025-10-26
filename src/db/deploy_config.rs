use crate::error::AppResult;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct DeployConfig {
    pub name: String,
    pub team: String,
    pub kind: String,
    pub config_repo_id: i64,
    pub artifact_repo_id: Option<i64>,
    pub active: bool,
}

impl DeployConfig {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(DeployConfig {
            name: row.get(0)?,
            team: row.get(1)?,
            kind: row.get(2)?,
            config_repo_id: row.get(3)?,
            artifact_repo_id: row.get(4)?,
            active: row.get(5)?,
        })
    }

    pub fn insert(
        deploy_config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT INTO deploy_config (name, team, kind, config_repo_id, artifact_repo_id, active) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
          .execute(params![deploy_config.name, deploy_config.team, deploy_config.kind, deploy_config.config_repo_id, deploy_config.artifact_repo_id, deploy_config.active])?;

        Ok(Self {
            name: deploy_config.name.clone(),
            team: deploy_config.team.clone(),
            kind: deploy_config.kind.clone(),
            config_repo_id: deploy_config.config_repo_id,
            artifact_repo_id: deploy_config.artifact_repo_id,
            active: deploy_config.active,
        })
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE deploy_config SET team = ?2, kind = ?3, config_repo_id = ?4, artifact_repo_id = ?5, active = ?6 WHERE name = ?1")?
          .execute(params![self.name, self.team, self.kind, self.config_repo_id, self.artifact_repo_id, self.active])?;

        Ok(())
    }
}
