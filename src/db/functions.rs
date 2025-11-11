use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};

// FIXME: These functions were from before we started to use the Repository / DAO object pattern.
// They're just here for now to help the existing code run.

use crate::{
    db::{git_branch::GitBranch, git_commit::GitCommit, git_repo::GitRepo},
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BranchWithCommits {
    pub branch: GitBranch,
    pub repo: GitRepo,
    pub commits: Vec<GitCommit>,
}

/// Get all branches with their recent commits (up to limit per branch)
pub fn get_branches_with_commits(
    conn: &PooledConnection<SqliteConnectionManager>,
    commit_limit: usize,
) -> AppResult<Vec<BranchWithCommits>> {
    let mut result = Vec::new();

    // First, get all branches with their repo info
    let query = r#"
        SELECT
            b.id, b.name, b.head_commit_sha, b.repo_id,
            r.name, r.owner_name, r.default_branch, r.private, r.language
        FROM git_branch b
        JOIN git_repo r ON b.repo_id = r.id
        ORDER BY b.id
    "#;

    let mut stmt = conn.prepare(query)?;
    let branch_rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,            // branch.id
            row.get::<_, String>(1)?,         // branch.name
            row.get::<_, String>(2)?,         // branch.head_commit_sha
            row.get::<_, i64>(3)?,            // branch.repo_id
            row.get::<_, String>(4)?,         // repo.name
            row.get::<_, String>(5)?,         // repo.owner_name
            row.get::<_, String>(6)?,         // repo.default_branch
            row.get::<_, bool>(7)?,           // repo.private
            row.get::<_, Option<String>>(8)?, // repo.language
        ))
    })?;

    for row in branch_rows {
        let (
            branch_id,
            _branch_name,
            _head_commit_sha,
            repo_id,
            _repo_name,
            _repo_owner,
            _default_branch,
            _is_private,
            _language,
        ) = row?;

        #[allow(clippy::expect_used)]
        let repo = GitRepo::get_by_id(&(repo_id as u64), conn)?.expect("Expect repo to be found");
        #[allow(clippy::expect_used)]
        let branch = GitBranch::get_by_id(branch_id, conn)?.expect("Expect branch to be found");

        // Get commits for this branch
        let commits_query = r#"
            SELECT c.id, c.sha, c.repo_id, c.message, c.author, c.committer, c.timestamp
            FROM git_commit c
            JOIN git_commit_branch cb ON c.id = cb.commit_id
            WHERE cb.branch_id = ?1
            ORDER BY c.timestamp DESC
            LIMIT ?2
        "#;

        let mut commits_stmt = conn.prepare(commits_query)?;
        let commit_rows = commits_stmt
            .query_and_then(params![branch_id, commit_limit], GitCommit::from_row)?
            .collect::<Result<Vec<_>, AppError>>()?;

        let commits = commit_rows;

        // Only include branches that have commits
        if !commits.is_empty() {
            result.push(BranchWithCommits {
                branch,
                repo,
                commits,
            });
        }
    }

    Ok(result)
}
