use crate::{
    db::{git_commit::GitCommit, ExistenceResult},
    error::{AppError, AppResult},
    web::BuildFilter,
};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitBranch {
    pub id: i64,
    pub name: String,
    pub head_commit_sha: String,
    pub repo_id: u64,
    pub active: bool,
}

pub struct GitBranchEgg {
    pub name: String,
    pub head_commit_sha: String,
    pub repo_id: u64,
    pub active: bool,
}

impl GitBranch {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitBranch {
            id: row.get(0)?,
            name: row.get(1)?,
            head_commit_sha: row.get(2)?,
            repo_id: row.get(3)?,
            active: row.get(4)?,
        })
    }

    pub fn from_egg(egg: &GitBranchEgg, id: i64) -> Self {
        Self {
            id,
            name: egg.name.clone(),
            head_commit_sha: egg.head_commit_sha.clone(),
            repo_id: egg.repo_id,
            active: egg.active,
        }
    }

    pub fn get_by_id(
        id: i64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let branch = conn
            .prepare(
                "SELECT id, name, head_commit_sha, repo_id, active FROM git_branch WHERE id = ?1",
            )?
            .query_row(params![id], |row| Ok(GitBranch::from_row(row)))
            .optional()
            .map_err(AppError::from)?
            .transpose()?;

        Ok(branch)
    }

    pub fn insert(
        branch: &GitBranchEgg,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT INTO git_branch (name, head_commit_sha, repo_id, active) VALUES (?1, ?2, ?3, ?4)")?
          .execute(params![branch.name, branch.head_commit_sha, branch.repo_id, branch.active])?;

        Ok(Self::from_egg(branch, conn.last_insert_rowid()))
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE git_branch SET name = ?2, head_commit_sha = ?3, repo_id = ?4, active = ?5 WHERE id = ?1")?
          .execute(params![self.id, self.name, self.head_commit_sha, self.repo_id, self.active])?;

        Ok(())
    }

    pub fn get_by_name(
        name: &str,
        repo_id: u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let branch = conn.prepare("SELECT id, name, head_commit_sha, repo_id, active FROM git_branch WHERE name = ?1 AND repo_id = ?2")?
          .query_row(params![name, repo_id], |row| {
            Ok(GitBranch::from_row(row))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(branch)
    }

    pub fn mark_inactive(
        &mut self,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        conn.prepare("UPDATE git_branch SET active = FALSE WHERE id = ?1")?
            .execute(params![self.id])?;

        self.active = false;

        Ok(())
    }

    pub fn latest_build(
        &self,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<GitCommit>> {
        let commit = GitCommit::get_by_sha(&self.head_commit_sha, self.repo_id, conn)
            .ok()
            .flatten();
        Ok(commit)
    }

    pub fn latest_completed_build(
        &self,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<GitCommit>> {
        // Get the latest completed build for this branch
        let commit = conn
            .prepare(
                r#"
                    SELECT c.id, c.sha, c.repo_id, c.message, c.author, c.committer, c.timestamp
                    FROM git_commit c
                    JOIN git_commit_branch cb ON c.id = cb.commit_id
                    JOIN git_commit_build cBuild ON c.id = cBuild.commit_id
                    WHERE cb.branch_id = ?1
                    AND cBuild.status IN ('Success', 'Failure')
                    ORDER BY c.timestamp DESC
                    LIMIT 1
                    "#,
            )?
            .query_row([self.id], |row| Ok(GitCommit::from_row(row)))
            .optional()?
            .transpose()?;

        Ok(commit)
    }

    pub fn latest_successful_build(
        &self,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<GitCommit>> {
        // Get the latest successful build for this branch
        let commit = conn
            .prepare(
                r#"
                    SELECT c.id, c.sha, c.repo_id, c.message, c.author, c.committer, c.timestamp
                    FROM git_commit c
                    JOIN git_commit_branch cb ON c.id = cb.commit_id
                    JOIN git_commit_build cBuild ON c.id = cBuild.commit_id
                    WHERE cb.branch_id = ?1
                    AND cBuild.status = 'Success'
                    ORDER BY c.timestamp DESC
                    LIMIT 1
                    "#,
            )?
            .query_row([self.id], |row| Ok(GitCommit::from_row(row)))
            .optional()?
            .transpose()?;

        Ok(commit)
    }

    pub fn search_build(
        &self,
        conn: &PooledConnection<SqliteConnectionManager>,
        build_filter: BuildFilter,
    ) -> AppResult<Option<GitCommit>> {
        let commit = match build_filter {
            BuildFilter::Any => self.latest_build(conn).ok().flatten(),
            BuildFilter::Completed => self.latest_completed_build(conn).ok().flatten(),
            BuildFilter::Successful => self.latest_successful_build(conn).ok().flatten(),
        };
        Ok(commit)
    }
}

impl GitBranchEgg {
    pub fn upsert(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<GitBranch> {
        let existing = conn
            .prepare("SELECT id FROM git_branch WHERE name = ?1 AND repo_id = ?2")?
            .query_row(params![self.name, self.repo_id], |row| {
                Ok(ExistenceResult { id: row.get(0)? })
            })
            .optional()?;

        if let Some(result) = existing {
            let branch = GitBranch::from_egg(self, result.id as i64);
            branch.update(conn)?;
            Ok(branch)
        } else {
            Ok(GitBranch::insert(self, conn)?)
        }
    }
}
