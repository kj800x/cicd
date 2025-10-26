use crate::{
    db::ExistenceResult,
    error::{AppError, AppResult},
};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

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

    pub fn insert(
        branch: &GitBranchEgg,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT INTO git_branch (name, head_commit_sha, repo_id, active) VALUES (?1, ?2, ?3, ?4)")?
          .execute(params![branch.name, branch.head_commit_sha, branch.repo_id, branch.active])?;

        Ok(Self::from_egg(branch, conn.last_insert_rowid() as i64))
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
            Ok(GitBranch::from_row(row).map_err(AppError::from))
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
