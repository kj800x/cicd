use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::{
    db::git_commit::GitCommit,
    error::{AppError, AppResult},
};

pub fn get_commits_since(
    conn: &PooledConnection<SqliteConnectionManager>,
    since: i64,
) -> AppResult<Vec<GitCommit>> {
    let commits = conn
        .prepare("SELECT id, sha, repo_id, message, author, committer, timestamp FROM git_commit WHERE timestamp > ?1 ORDER BY timestamp DESC")?
        .query_and_then(params![since], GitCommit::from_row)?
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(commits)
}
