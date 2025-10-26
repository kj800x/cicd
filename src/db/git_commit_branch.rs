use crate::error::AppResult;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct GitCommitBranch {
    pub commit_id: i64,
    pub branch_id: i64,
}

impl GitCommitBranch {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitCommitBranch {
            commit_id: row.get(0)?,
            branch_id: row.get(1)?,
        })
    }

    pub fn insert(
        branch: &GitCommitBranch,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT INTO git_commit_branch (commit_id, branch_id) VALUES (?1, ?2)")?
            .execute(params![branch.commit_id, branch.branch_id])?;

        Ok(Self {
            commit_id: branch.commit_id,
            branch_id: branch.branch_id,
        })
    }
}
