use crate::error::AppResult;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct DeployConfigVersion {
    pub name: String,
    pub config_repo_id: u64,
    pub config_commit_sha: String,
    pub hash: String,
}

impl DeployConfigVersion {
    pub fn upsert(
        deploy_config_version: &DeployConfigVersion,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        conn.prepare("INSERT OR REPLACE INTO deploy_config_version (name, config_repo_id, config_commit_sha, hash) VALUES (?1, ?2, ?3, ?4)")?
          .execute(params![deploy_config_version.name, deploy_config_version.config_repo_id, deploy_config_version.config_commit_sha, deploy_config_version.hash])?;

        Ok(())
    }
}
