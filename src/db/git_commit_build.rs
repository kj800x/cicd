use crate::error::{AppError, AppResult};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct GitCommitBuild {
    pub repo_id: u64,
    pub commit_id: i64,
    pub check_name: String,
    pub status: String,
    pub url: String,
}

impl GitCommitBuild {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitCommitBuild {
            repo_id: row.get(0)?,
            commit_id: row.get(1)?,
            check_name: row.get(2)?,
            status: row.get(3)?,
            url: row.get(4)?,
        })
    }

    pub fn get_by_commit_id(
        commit_id: &i64,
        repo_id: &u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<GitCommitBuild>> {
        let build = conn.prepare("SELECT repo_id, commit_id, check_name, status, url FROM git_commit_build WHERE commit_id = ?1 AND repo_id = ?2")?
          .query_row(params![commit_id, repo_id], |row| {
            Ok(GitCommitBuild::from_row(row).map_err(AppError::from))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(build)
    }

    pub fn upsert(
        build: &GitCommitBuild,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT OR REPLACE INTO git_commit_build (repo_id, commit_id, check_name, status, url) VALUES (?1, ?2, ?3, ?4, ?5)")?
            .execute(params![build.repo_id, build.commit_id, build.check_name, build.status, build.url])?;

        Ok(Self {
            repo_id: build.repo_id,
            commit_id: build.commit_id,
            check_name: build.check_name.clone(),
            status: build.status.clone(),
            url: build.url.clone(),
        })
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE git_commit_build SET status = ?2, url = ?3 WHERE repo_id = ?1 AND commit_id = ?4 AND check_name = ?5")?
            .execute(params![self.repo_id, self.commit_id, self.check_name, self.status, self.url])?;

        Ok(())
    }
}
