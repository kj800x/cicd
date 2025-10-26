use crate::error::AppResult;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct GitCommitParent {
    pub commit_id: i64,
    pub parent_sha: String,
}

impl GitCommitParent {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitCommitParent {
            commit_id: row.get(0)?,
            parent_sha: row.get(1)?,
        })
    }

    pub fn upsert(
        parent: &GitCommitParent,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare(
            "INSERT OR REPLACE INTO git_commit_parent (commit_id, parent_sha) VALUES (?1, ?2)",
        )?
        .execute(params![parent.commit_id, parent.parent_sha.clone()])?;

        Ok(Self {
            commit_id: parent.commit_id,
            parent_sha: parent.parent_sha.clone(),
        })
    }
}
