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
    /// GitHub App id that produced this check run (e.g. 15368 for GitHub
    /// Actions). Nullable: legacy rows and the collapsed legacy-status entry
    /// have no single app. Lets deploy configs eventually depend on specific
    /// checks keyed by (app_id, check name).
    pub app_id: Option<u64>,
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
            app_id: row.get::<_, Option<u64>>(7)?,
        })
    }

    pub fn get_all_by_commit_id(
        commit_id: &i64,
        repo_id: &u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Vec<GitCommitBuild>> {
        let builds = conn.prepare("SELECT repo_id, commit_id, check_name, status, url, start_time, settle_time, app_id FROM git_commit_build WHERE commit_id = ?1 AND repo_id = ?2")?
          .query_and_then(params![commit_id, repo_id], |row| {
            GitCommitBuild::from_row(row)
          })?
          .collect::<AppResult<Vec<GitCommitBuild>>>()?;

        Ok(builds)
    }

    /// Returns a single aggregated build status for a commit.
    /// If any build is pending, the aggregate is pending.
    /// If any build failed, the aggregate is failure.
    /// Only if all builds succeeded is the aggregate success.
    pub fn get_aggregate_by_commit_id(
        commit_id: &i64,
        repo_id: &u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<GitCommitBuild>> {
        let builds = Self::get_all_by_commit_id(commit_id, repo_id, conn)?;
        if builds.is_empty() {
            return Ok(None);
        }

        let any_pending = builds.iter().any(|b| b.status == "Pending");
        let any_failure = builds.iter().any(|b| b.status == "Failure");
        let all_none = builds.iter().all(|b| b.status == "None");

        let status = if any_failure {
            "Failure"
        } else if any_pending {
            "Pending"
        } else if all_none {
            "None"
        } else {
            "Success"
        };

        // Use the first build as a base, with the aggregated status.
        // For url, prefer the failing or pending build's url, otherwise use the first.
        let representative = builds
            .iter()
            .find(|b| b.status == "Failure")
            .or_else(|| builds.iter().find(|b| b.status == "Pending"))
            .unwrap_or(&builds[0]);

        // start_time: earliest start across all builds
        let start_time = builds.iter().filter_map(|b| b.start_time).min();
        // settle_time: latest settle, but only if no builds are still pending
        let settle_time = if any_pending {
            None
        } else {
            builds.iter().filter_map(|b| b.settle_time).max()
        };

        Ok(Some(GitCommitBuild {
            repo_id: *repo_id,
            commit_id: *commit_id,
            check_name: "aggregate".to_string(),
            status: status.to_string(),
            url: representative.url.clone(),
            start_time,
            settle_time,
            // The aggregate spans every check, so it has no single producing app.
            app_id: None,
        }))
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

        conn.prepare("INSERT OR REPLACE INTO git_commit_build (repo_id, commit_id, check_name, status, url, start_time, settle_time, app_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?
            .execute(params![build.repo_id, build.commit_id, build.check_name, build.status, build.url, existing_start_time.or(build.start_time), build.settle_time, build.app_id])?;

        Ok(Self {
            repo_id: build.repo_id,
            commit_id: build.commit_id,
            check_name: build.check_name.clone(),
            status: build.status.clone(),
            url: build.url.clone(),
            start_time: existing_start_time.or(build.start_time),
            settle_time: build.settle_time,
            app_id: build.app_id,
        })
    }

    /// Make the stored builds for a commit match `builds` exactly.
    ///
    /// Upserts every build in `builds`, then deletes any existing rows for the
    /// commit whose `check_name` is not present in `builds`. This is what makes
    /// a scan authoritative: stale rows (e.g. an old per-suite phantom that
    /// GitHub no longer reports, or a check that was removed) are pruned instead
    /// of lingering and pinning the aggregate.
    ///
    /// If `builds` is empty, all rows for the commit are removed.
    pub fn reconcile_for_commit(
        repo_id: u64,
        commit_id: i64,
        builds: &[GitCommitBuild],
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<()> {
        for build in builds {
            debug_assert_eq!(build.repo_id, repo_id);
            debug_assert_eq!(build.commit_id, commit_id);
            Self::upsert(build, conn)?;
        }

        if builds.is_empty() {
            conn.prepare("DELETE FROM git_commit_build WHERE commit_id = ?1 AND repo_id = ?2")?
                .execute(params![commit_id, repo_id])?;
            return Ok(());
        }

        // Delete rows for this commit whose check_name isn't in the kept set.
        // SQLite's parameter limit is high enough that listing names inline via
        // carrying-bound placeholders is fine for realistic check counts.
        let placeholders = vec!["?"; builds.len()].join(", ");
        let sql = format!(
            "DELETE FROM git_commit_build \
             WHERE commit_id = ?1 AND repo_id = ?2 \
             AND check_name NOT IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&commit_id, &repo_id];
        for build in builds {
            bound.push(&build.check_name);
        }
        stmt.execute(bound.as_slice())?;

        Ok(())
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
