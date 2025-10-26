use crate::{
    db::git_commit_parent::GitCommitParent,
    error::{AppError, AppResult},
};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

pub struct GitCommit {
    pub id: i64,
    pub sha: String,
    pub repo_id: u64,
    pub message: String,
    pub author: String,
    pub committer: String,
    pub timestamp: i64,
}

pub struct GitCommitEgg {
    pub sha: String,
    pub repo_id: u64,
    pub message: String,
    pub author: String,
    pub committer: String,
    pub timestamp: i64,
}

impl GitCommit {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitCommit {
            id: row.get(0)?,
            sha: row.get(1)?,
            repo_id: row.get(2)?,
            message: row.get(3)?,
            author: row.get(4)?,
            committer: row.get(5)?,
            timestamp: row.get(6)?,
        })
    }

    pub fn from_egg(egg: &GitCommitEgg, id: i64) -> Self {
        Self {
            id,
            sha: egg.sha.clone(),
            repo_id: egg.repo_id,
            message: egg.message.clone(),
            author: egg.author.clone(),
            committer: egg.committer.clone(),
            timestamp: egg.timestamp,
        }
    }

    pub fn get_by_sha(
        sha: &str,
        repo_id: u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let commit = conn.prepare("SELECT id, sha, repo_id, message, author, committer, timestamp FROM git_commit WHERE sha = ?1 AND repo_id = ?2")?
          .query_row(params![sha, repo_id], |row| {
            Ok(GitCommit::from_row(row).map_err(AppError::from))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(commit)
    }

    pub fn upsert(
        commit: &GitCommitEgg,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare(
            "INSERT OR REPLACE INTO git_commit (sha, repo_id, message, author, committer, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![
            commit.sha,
            commit.repo_id,
            commit.message,
            commit.author,
            commit.committer,
            commit.timestamp
        ])?;

        Ok(Self::from_egg(commit, conn.last_insert_rowid() as i64))
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE git_commit SET sha = ?2, repo_id = ?3, message = ?4, author = ?5, committer = ?6, timestamp = ?7 WHERE id = ?1")?
          .execute(params![self.id, self.sha, self.repo_id, self.message, self.author, self.committer, self.timestamp])?;

        Ok(())
    }

    pub fn add_branch(
        &self,
        branch_id: i64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        conn.prepare(
            "INSERT OR REPLACE INTO git_commit_branch (commit_id, branch_id) VALUES (?1, ?2)",
        )?
        .execute(params![self.id, branch_id])?;

        Ok(())
    }

    pub fn add_parent_shas(
        &self,
        parent_shas: Vec<String>,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        for parent_sha in parent_shas {
            GitCommitParent::upsert(
                &GitCommitParent {
                    commit_id: self.id,
                    parent_sha,
                },
                conn,
            )?;
        }
        Ok(())
    }
}
