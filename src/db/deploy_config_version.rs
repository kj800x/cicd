use crate::error::{AppError, AppResult};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

pub struct DeployConfigVersion {
    pub name: String,
    pub config_repo_id: u64,
    pub config_commit_sha: String,
    pub hash: String,
}

impl DeployConfigVersion {
    #[allow(unused)]
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(DeployConfigVersion {
            name: row.get(0)?,
            config_repo_id: row.get(1)?,
            config_commit_sha: row.get(2)?,
            hash: row.get(3)?,
        })
    }

    #[allow(unused)]
    pub fn get(
        name: &str,
        config_repo_id: u64,
        config_commit_sha: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let deploy_config_version = conn.prepare("SELECT name, config_repo_id, config_commit_sha, hash FROM deploy_config_version WHERE name = ?1 AND config_repo_id = ?2 AND config_commit_sha = ?3")?
          .query_row(params![name, config_repo_id, config_commit_sha], |row| {
            Ok(DeployConfigVersion::from_row(row))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(deploy_config_version)
    }

    pub fn upsert(
        deploy_config_version: &DeployConfigVersion,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        conn.prepare("INSERT OR REPLACE INTO deploy_config_version (name, config_repo_id, config_commit_sha, hash) VALUES (?1, ?2, ?3, ?4)")?
          .execute(params![deploy_config_version.name, deploy_config_version.config_repo_id, deploy_config_version.config_commit_sha, deploy_config_version.hash])?;

        Ok(())
    }
}
