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
    pub start_time: Option<u64>,
    pub settle_time: Option<u64>,
}

impl GitCommitBuild {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitCommitBuild {
            repo_id: row.get(0)?,
            commit_id: row.get(1)?,
            check_name: row.get(2)?,
            status: row.get(3)?,
            url: row.get(4)?,
            start_time: row.get::<_, Option<u64>>(5)?,
            settle_time: row.get::<_, Option<u64>>(6)?,
        })
    }

    pub fn get_by_commit_id(
        commit_id: &i64,
        repo_id: &u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<GitCommitBuild>> {
        let build = conn.prepare("SELECT repo_id, commit_id, check_name, status, url, start_time, settle_time FROM git_commit_build WHERE commit_id = ?1 AND repo_id = ?2")?
          .query_row(params![commit_id, repo_id], |row| {
            Ok(GitCommitBuild::from_row(row))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(build)
    }

    pub fn upsert(
        build: &GitCommitBuild,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        let existing_start_time = conn.prepare("SELECT start_time FROM git_commit_build WHERE commit_id = ?1 AND repo_id = ?2 AND check_name = ?3")?
            .query_row(params![build.commit_id, build.repo_id, build.check_name], |row| {
                Ok(row.get::<_, Option<u64>>(0))
            })
            .optional()
            .map_err(AppError::from)?
            .transpose()?.flatten();

        conn.prepare("INSERT OR REPLACE INTO git_commit_build (repo_id, commit_id, check_name, status, url, start_time, settle_time) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?
            .execute(params![build.repo_id, build.commit_id, build.check_name, build.status, build.url, existing_start_time.or(build.start_time), build.settle_time])?;

        Ok(Self {
            repo_id: build.repo_id,
            commit_id: build.commit_id,
            check_name: build.check_name.clone(),
            status: build.status.clone(),
            url: build.url.clone(),
            start_time: existing_start_time.or(build.start_time),
            settle_time: build.settle_time,
        })
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE git_commit_build SET status = ?2, url = ?3, start_time = ?4, settle_time = ?5 WHERE repo_id = ?1 AND commit_id = ?6 AND check_name = ?7")?
            .execute(params![self.repo_id, self.status, self.url, self.start_time, self.settle_time, self.commit_id, self.check_name])?;

        Ok(())
    }

    /// Returns the average build duration in milliseconds for the last `limit` completed builds
    /// for the given repo (builds with both start_time and settle_time set).
    pub fn avg_build_duration_ms(
        repo_id: u64,
        limit: u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<u64>> {
        let durations: Vec<u64> = conn
            .prepare(
                "SELECT start_time, settle_time FROM git_commit_build \
                 WHERE repo_id = ?1 \
                 AND start_time IS NOT NULL AND settle_time IS NOT NULL \
                 AND settle_time > start_time \
                 ORDER BY settle_time DESC \
                 LIMIT ?2",
            )?
            .query_map(params![repo_id, limit], |row| {
                let start: u64 = row.get(0)?;
                let settle: u64 = row.get(1)?;
                Ok(settle - start)
            })?
            .filter_map(|r| r.ok())
            .collect();

        if durations.is_empty() {
            return Ok(None);
        }

        Ok(Some(durations.iter().sum::<u64>() / durations.len() as u64))
    }
}
