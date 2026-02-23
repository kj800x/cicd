use crate::error::AppResult;
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
    pub fn upsert(
        deploy_config_version: &DeployConfigVersion,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        conn.prepare("INSERT OR REPLACE INTO deploy_config_version (name, config_repo_id, config_commit_sha, hash) VALUES (?1, ?2, ?3, ?4)")?
          .execute(params![deploy_config_version.name, deploy_config_version.config_repo_id, deploy_config_version.config_commit_sha, deploy_config_version.hash])?;

        Ok(())
    }

    /// Look up the manifest hash for a given deploy config name, config repo, and commit SHA.
    /// Returns None if no entry exists (e.g. not yet synced).
    pub fn get_hash(
        name: &str,
        config_repo_id: u64,
        commit_sha: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<String>> {
        Ok(conn
            .prepare(
                "SELECT hash FROM deploy_config_version \
                 WHERE name = ?1 AND config_repo_id = ?2 AND config_commit_sha = ?3",
            )?
            .query_row(params![name, config_repo_id, commit_sha], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }
}
