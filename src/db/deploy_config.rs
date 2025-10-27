use crate::error::{AppError, AppResult};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

pub struct DeployConfig {
    pub name: String,
    pub team: String,
    pub kind: String,
    pub config_repo_id: u64,
    pub artifact_repo_id: Option<u64>,
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

    pub fn get_by_name(
        name: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let deploy_config = conn.prepare("SELECT name, team, kind, config_repo_id, artifact_repo_id, active FROM deploy_config WHERE name = ?1")?
          .query_row(params![name], |row| {
            Ok(DeployConfig::from_row(row).map_err(AppError::from))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(deploy_config)
    }

    pub fn get_by_config_repo_id(
        config_repo_id: u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Vec<Self>> {
        let mut deploy_configs = Vec::new();
        let mut stmt = conn.prepare("SELECT name, team, kind, config_repo_id, artifact_repo_id, active FROM deploy_config WHERE config_repo_id = ?1")?;
        let mut rows = stmt.query(params![config_repo_id])?;

        while let Some(row) = rows.next()? {
            deploy_configs.push(DeployConfig::from_row(&row)?);
        }

        Ok(deploy_configs)
    }

    pub fn upsert(
        deploy_config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT OR REPLACE  INTO deploy_config (name, team, kind, config_repo_id, artifact_repo_id, active) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
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

    pub fn mark_inactive(
        name: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        conn.prepare("UPDATE deploy_config SET active = FALSE WHERE name = ?1")?
            .execute(params![name])?;

        Ok(())
    }
}
