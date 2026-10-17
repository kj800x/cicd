//! Deploy runs: the lifecycle of one deploy, from the moment its revision
//! is recorded until cicd knows whether it worked.
//!
//! A revision is the immutable record of what was applied; a run is the
//! mutable story of that deploy afterwards. An autodeploy that a
//! pre-check holds starts as a `WAITING` run with no revision, asked again
//! on an interval until its checks pass and it deploys. A run is
//! `ROLLING_OUT` once its revision is recorded, and the driver loop
//! (`crate::deploy_runs`) moves it on: to `SUCCEEDED` once every object
//! the config owns has rolled out, or to `FAILED` when a rollout stalls,
//! crash-loops or misses its deadline. A newer deploy of the same config
//! moves every run still in progress to `SUPERSEDED`.
//!
//! Everything lives here rather than in memory, so a cicd restart
//! (including one caused by deploying cicd) picks up where it left off.
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use chrono::Utc;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension, Row};

use crate::db::revision::Revision;
use crate::error::{AppError, AppResult};

/// How long a run may stay in `ROLLING_OUT` before it fails, unless the
/// config sets `checks.rolloutDeadline`.
pub const DEFAULT_ROLLOUT_DEADLINE_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    /// An autodeploy a pre-check held; it has no revision yet.
    Waiting,
    /// The revision is recorded; waiting for the cluster to catch up.
    RollingOut,
    /// Rolled out; waiting for the post-checks' final answers.
    Monitoring,
    /// Everything the config owns rolled out (and every post-check said OK).
    Succeeded,
    /// Rolled out, and a post-check warned.
    SucceededWithWarnings,
    /// The rollout stalled, crash-looped or missed its deadline.
    Failed,
    /// Failed, and cicd rolled the config back to the last run that
    /// succeeded.
    RolledBack,
    /// A newer deploy of the same config replaced this run before it ended.
    Superseded,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Waiting => "WAITING",
            RunState::RollingOut => "ROLLING_OUT",
            RunState::Monitoring => "MONITORING",
            RunState::Succeeded => "SUCCEEDED",
            RunState::SucceededWithWarnings => "SUCCEEDED_WITH_WARNINGS",
            RunState::Failed => "FAILED",
            RunState::RolledBack => "ROLLED_BACK",
            RunState::Superseded => "SUPERSEDED",
        }
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunState {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "WAITING" => Ok(RunState::Waiting),
            "ROLLING_OUT" => Ok(RunState::RollingOut),
            "MONITORING" => Ok(RunState::Monitoring),
            "SUCCEEDED_WITH_WARNINGS" => Ok(RunState::SucceededWithWarnings),
            "SUCCEEDED" => Ok(RunState::Succeeded),
            "FAILED" => Ok(RunState::Failed),
            "ROLLED_BACK" => Ok(RunState::RolledBack),
            "SUPERSEDED" => Ok(RunState::Superseded),
            other => Err(AppError::Internal(format!(
                "unknown deploy run state {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployRun {
    pub id: i64,
    pub config_name: String,
    /// What started the deploy: `web`, `mcp`, `autodeploy`, `rollback`, or
    /// `backfill` for the runs seeded from revisions when runs were added.
    pub trigger: String,
    pub state: RunState,
    pub revision_id: Option<i64>,
    /// The latest word on the run: what it is waiting for while it rolls
    /// out, why it failed once it has.
    pub message: Option<String>,
    /// Milliseconds since the epoch, like every other timestamp here.
    pub created_at: i64,
    pub rolled_out_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub deadline_at: Option<i64>,
    pub superseded_by: Option<i64>,
    /// The soft-blocking pre-checks a person acknowledged to deploy anyway.
    pub acknowledged: Vec<String>,
    /// When a waiting run asks its pre-checks again.
    pub next_check_at: Option<i64>,
    /// When a long wait was reported to Discord.
    pub notified_at: Option<i64>,
}

const COLUMNS: &str = "id, config_name, trigger, state, revision_id, message, created_at, rolled_out_at, finished_at, deadline_at, superseded_by, acknowledged, next_check_at, notified_at";

/// The states a newer deploy of the same config supersedes.
const UNFINISHED: &str = "('WAITING', 'ROLLING_OUT', 'MONITORING')";

impl DeployRun {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        let state: String = row.get(3)?;
        Ok(DeployRun {
            id: row.get(0)?,
            config_name: row.get(1)?,
            trigger: row.get(2)?,
            state: state.parse().map_err(|e: AppError| {
                rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into())
            })?,
            revision_id: row.get(4)?,
            message: row.get(5)?,
            created_at: row.get(6)?,
            rolled_out_at: row.get(7)?,
            finished_at: row.get(8)?,
            deadline_at: row.get(9)?,
            superseded_by: row.get(10)?,
            acknowledged: row
                .get::<_, Option<String>>(11)?
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
            next_check_at: row.get(12)?,
            notified_at: row.get(13)?,
        })
    }

    /// Bookkeeping after a revision is recorded. A deploy (or a patch
    /// change) starts a run in `ROLLING_OUT` that supersedes the config's
    /// unfinished runs; an undeploy, or a revision with nothing deployed,
    /// only supersedes them, since there is nothing left to roll out.
    /// The new run fails if it has not rolled out within `deadline_ms`.
    /// Returns the new run, if one was started.
    #[cfg(test)]
    pub fn after_revision(
        conn: &PooledConnection<SqliteConnectionManager>,
        revision: &Revision,
        trigger: &str,
        deadline_ms: i64,
    ) -> AppResult<Option<Self>> {
        Self::after_revision_with(conn, revision, trigger, deadline_ms, None, &[])
    }

    /// [`DeployRun::after_revision`], continuing the `waiting` run when this
    /// deploy is that held autodeploy going ahead (it keeps its id and
    /// history instead of a new run superseding it), and recording which
    /// soft blocks were `acknowledged`.
    pub fn after_revision_with(
        conn: &PooledConnection<SqliteConnectionManager>,
        revision: &Revision,
        trigger: &str,
        deadline_ms: i64,
        waiting: Option<i64>,
        acknowledged: &[String],
    ) -> AppResult<Option<Self>> {
        let now = Utc::now().timestamp_millis();
        let tx = conn.unchecked_transaction()?;
        let deploys = revision.action != "undeploy" && revision.config_sha.is_some();
        let acknowledged_json = (!acknowledged.is_empty())
            .then(|| serde_json::to_string(acknowledged).unwrap_or_default());
        let id = if deploys {
            let deadline_at = now + deadline_ms;
            let continued = match waiting {
                Some(id) => tx.execute(
                    "UPDATE deploy_run SET state = ?1, revision_id = ?2, deadline_at = ?3, message = NULL, \
                     next_check_at = NULL, acknowledged = COALESCE(?4, acknowledged) \
                     WHERE id = ?5 AND config_name = ?6 AND state = ?7",
                    params![
                        RunState::RollingOut.as_str(),
                        revision.id,
                        deadline_at,
                        acknowledged_json,
                        id,
                        revision.config_name,
                        RunState::Waiting.as_str()
                    ],
                )? == 1,
                None => false,
            };
            if continued {
                waiting
            } else {
                tx.execute(
                    "INSERT INTO deploy_run (config_name, trigger, state, revision_id, created_at, deadline_at, acknowledged) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        revision.config_name,
                        trigger,
                        RunState::RollingOut.as_str(),
                        revision.id,
                        now,
                        deadline_at,
                        acknowledged_json
                    ],
                )?;
                Some(tx.last_insert_rowid())
            }
        } else {
            None
        };
        tx.execute(
            &format!(
                "UPDATE deploy_run SET state = ?1, superseded_by = ?2, finished_at = ?3 \
                 WHERE config_name = ?4 AND state IN {UNFINISHED} AND id != COALESCE(?2, -1)"
            ),
            params![RunState::Superseded.as_str(), id, now, revision.config_name],
        )?;
        let run = match id {
            Some(id) => tx
                .query_row(
                    &format!("SELECT {COLUMNS} FROM deploy_run WHERE id = ?1"),
                    params![id],
                    Self::from_row,
                )
                .optional()?,
            None => None,
        };
        tx.commit()?;
        Ok(run)
    }

    /// Hold an autodeploy: a `WAITING` run that asks its pre-checks again
    /// at `next_check_at`. A config has at most one: an older waiting run
    /// is superseded by this one, since this one deploys a newer latest.
    pub fn wait(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
        trigger: &str,
        message: &str,
        next_check_at: i64,
    ) -> AppResult<Self> {
        let now = Utc::now().timestamp_millis();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO deploy_run (config_name, trigger, state, message, created_at, next_check_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                config_name,
                trigger,
                RunState::Waiting.as_str(),
                message,
                now,
                next_check_at
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE deploy_run SET state = ?1, superseded_by = ?2, finished_at = ?3 \
             WHERE config_name = ?4 AND state = ?5 AND id != ?2",
            params![
                RunState::Superseded.as_str(),
                id,
                now,
                config_name,
                RunState::Waiting.as_str()
            ],
        )?;
        let run = tx.query_row(
            &format!("SELECT {COLUMNS} FROM deploy_run WHERE id = ?1"),
            params![id],
            Self::from_row,
        )?;
        tx.commit()?;
        Ok(run)
    }

    /// A waiting run was asked again and is still held.
    pub fn keep_waiting(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
        message: &str,
        next_check_at: i64,
    ) -> AppResult<()> {
        conn.execute(
            "UPDATE deploy_run SET message = ?1, next_check_at = ?2 WHERE id = ?3 AND state = ?4",
            params![message, next_check_at, id, RunState::Waiting.as_str()],
        )?;
        Ok(())
    }

    /// Stop a waiting run without deploying: autodeploy was turned off, a
    /// blocker was added, or the config is gone.
    pub fn cancel_waiting(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
        why: &str,
    ) -> AppResult<()> {
        conn.execute(
            "UPDATE deploy_run SET state = ?1, message = ?2, finished_at = ?3, next_check_at = NULL \
             WHERE id = ?4 AND state = ?5",
            params![
                RunState::Superseded.as_str(),
                why,
                Utc::now().timestamp_millis(),
                id,
                RunState::Waiting.as_str()
            ],
        )?;
        Ok(())
    }

    /// Record that a long wait was reported.
    pub fn mark_notified(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
    ) -> AppResult<()> {
        conn.execute(
            "UPDATE deploy_run SET notified_at = ?1 WHERE id = ?2",
            params![Utc::now().timestamp_millis(), id],
        )?;
        Ok(())
    }

    /// Every waiting run, oldest first.
    pub fn waiting(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM deploy_run WHERE state = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![RunState::Waiting.as_str()], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn get(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
    ) -> AppResult<Option<Self>> {
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM deploy_run WHERE id = ?1"),
                params![id],
                Self::from_row,
            )
            .optional()?)
    }

    /// Every run that has not ended, oldest first: what the driver loop
    /// looks at.
    pub fn unfinished(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM deploy_run WHERE state = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![RunState::RollingOut.as_str()], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The run each of `revision_ids` started, keyed by revision id.
    /// Revisions that started no run (undeploys, revisions from before runs
    /// existed) are absent.
    pub fn for_revisions(
        conn: &PooledConnection<SqliteConnectionManager>,
        revision_ids: &[i64],
    ) -> AppResult<HashMap<i64, Self>> {
        let mut runs = HashMap::new();
        if revision_ids.is_empty() {
            return Ok(runs);
        }
        let placeholders = vec!["?"; revision_ids.len()].join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM deploy_run WHERE revision_id IN ({placeholders}) ORDER BY id"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(revision_ids.iter()),
            Self::from_row,
        )?;
        for run in rows {
            let run = run?;
            if let Some(revision_id) = run.revision_id {
                // Ordered by id, so the newest run for a revision wins.
                runs.insert(revision_id, run);
            }
        }
        Ok(runs)
    }

    /// A config's most recent runs, newest first.
    #[cfg(test)]
    pub fn list_for(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
        limit: usize,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM deploy_run WHERE config_name = ?1 ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![config_name, limit as i64], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The newest run of a config, older than `before`, that succeeded with
    /// a revision other than `not_revision`: what a failed run rolls back
    /// to. A run that redeployed the failed revision is skipped, since that
    /// revision is the one that just failed.
    pub fn last_good(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
        before: i64,
        not_revision: Option<i64>,
    ) -> AppResult<Option<Self>> {
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM deploy_run \
                     WHERE config_name = ?1 AND id < ?2 AND state IN (?3, 'SUCCEEDED_WITH_WARNINGS') AND revision_id IS NOT NULL \
                     AND revision_id != COALESCE(?4, -1) \
                     ORDER BY id DESC LIMIT 1"
                ),
                params![
                    config_name,
                    before,
                    RunState::Succeeded.as_str(),
                    not_revision
                ],
                Self::from_row,
            )
            .optional()?)
    }

    /// Mark a failed run as rolled back, once the rollback is under way.
    pub fn mark_rolled_back(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
    ) -> AppResult<()> {
        conn.execute(
            "UPDATE deploy_run SET state = ?1 WHERE id = ?2 AND state = ?3",
            params![RunState::RolledBack.as_str(), id, RunState::Failed.as_str()],
        )?;
        Ok(())
    }

    /// Record what a rolling-out run is waiting for. Only touches a run
    /// that is still rolling out, so a run that ended meanwhile keeps its
    /// final message.
    pub fn set_progress(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
        message: &str,
    ) -> AppResult<()> {
        conn.execute(
            "UPDATE deploy_run SET message = ?1 WHERE id = ?2 AND state = ?3",
            params![message, id, RunState::RollingOut.as_str()],
        )?;
        Ok(())
    }

    /// End a rolling-out run as `SUCCEEDED` or `FAILED`. Returns whether
    /// this call ended it: a run that was superseded meanwhile is left
    /// alone, so the caller does not act on a stale verdict.
    pub fn finish(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
        state: RunState,
        message: Option<&str>,
    ) -> AppResult<bool> {
        if !matches!(
            state,
            RunState::Succeeded | RunState::SucceededWithWarnings | RunState::Failed
        ) {
            return Err(AppError::Internal(format!(
                "a run cannot finish as {state}"
            )));
        }
        let now = Utc::now().timestamp_millis();
        let rolled_out_at = (state != RunState::Failed).then_some(now);
        let changed = conn.execute(
            "UPDATE deploy_run SET state = ?1, message = ?2, finished_at = ?3, rolled_out_at = COALESCE(rolled_out_at, ?4) \
             WHERE id = ?5 AND state IN ('ROLLING_OUT', 'MONITORING')",
            params![state.as_str(), message, now, rolled_out_at, id],
        )?;
        Ok(changed == 1)
    }

    /// A run rolled out and has post-checks: it monitors until they answer.
    /// Returns whether this call moved it (a superseded run is left alone).
    pub fn monitor(conn: &PooledConnection<SqliteConnectionManager>, id: i64) -> AppResult<bool> {
        let changed = conn.execute(
            "UPDATE deploy_run SET state = ?1, rolled_out_at = ?2, message = NULL WHERE id = ?3 AND state = ?4",
            params![
                RunState::Monitoring.as_str(),
                Utc::now().timestamp_millis(),
                id,
                RunState::RollingOut.as_str()
            ],
        )?;
        Ok(changed == 1)
    }

    /// Every run waiting on post-checks, oldest first.
    pub fn monitoring(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM deploy_run WHERE state = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![RunState::Monitoring.as_str()], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::revision::NewRevision;
    use crate::db::test_support::migrated_memory_pool;

    fn revision(
        conn: &PooledConnection<SqliteConnectionManager>,
        config: &str,
        action: &str,
    ) -> AppResult<Revision> {
        Revision::record(
            conn,
            NewRevision {
                config_name: config.to_string(),
                actor: "web".to_string(),
                action: action.to_string(),
                reason: None,
                config_sha: (action != "undeploy").then(|| "c1".to_string()),
                config_branch: None,
                config_version_hash: None,
                patches: None,
                temporary: false,
                parameters: vec![],
            },
        )
    }

    #[test]
    fn a_deploy_starts_a_run_and_supersedes_the_unfinished_one() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let first = DeployRun::after_revision(
            &conn,
            &revision(&conn, "site", "deploy")?,
            "web",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?
        .expect("a deploy starts a run");
        assert_eq!(first.state, RunState::RollingOut);
        assert!(first.deadline_at.is_some());

        let other = DeployRun::after_revision(
            &conn,
            &revision(&conn, "other", "deploy")?,
            "mcp",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?
        .expect("a deploy starts a run");
        let second = DeployRun::after_revision(
            &conn,
            &revision(&conn, "site", "deploy")?,
            "autodeploy",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?
        .expect("a deploy starts a run");

        let first = DeployRun::get(&conn, first.id)?.expect("still there");
        assert_eq!(first.state, RunState::Superseded);
        assert_eq!(first.superseded_by, Some(second.id));
        assert!(first.finished_at.is_some());
        // Other configs' runs are untouched.
        assert_eq!(
            DeployRun::get(&conn, other.id)?.map(|r| r.state),
            Some(RunState::RollingOut)
        );
        assert_eq!(
            DeployRun::unfinished(&conn)?
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![other.id, second.id]
        );
        Ok(())
    }

    #[test]
    fn an_undeploy_only_supersedes() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = DeployRun::after_revision(
            &conn,
            &revision(&conn, "site", "deploy")?,
            "web",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?
        .expect("a deploy starts a run");
        let none = DeployRun::after_revision(
            &conn,
            &revision(&conn, "site", "undeploy")?,
            "web",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?;
        assert!(none.is_none());
        let run = DeployRun::get(&conn, run.id)?.expect("still there");
        assert_eq!(run.state, RunState::Superseded);
        assert_eq!(run.superseded_by, None);
        Ok(())
    }

    #[test]
    fn finish_ends_only_a_rolling_out_run() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let run = DeployRun::after_revision(
            &conn,
            &revision(&conn, "site", "deploy")?,
            "web",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?
        .expect("a deploy starts a run");
        DeployRun::set_progress(&conn, run.id, "waiting for Deployment/site")?;
        assert!(DeployRun::finish(&conn, run.id, RunState::Succeeded, None)?);
        let done = DeployRun::get(&conn, run.id)?.expect("still there");
        assert_eq!(done.state, RunState::Succeeded);
        assert!(done.rolled_out_at.is_some());
        assert_eq!(done.message, None);

        // A second verdict, or progress, no longer lands.
        assert!(!DeployRun::finish(
            &conn,
            run.id,
            RunState::Failed,
            Some("late")
        )?);
        DeployRun::set_progress(&conn, run.id, "late")?;
        let done = DeployRun::get(&conn, run.id)?.expect("still there");
        assert_eq!(done.state, RunState::Succeeded);
        assert_eq!(done.message, None);
        assert!(DeployRun::finish(&conn, run.id, RunState::Superseded, None).is_err());
        Ok(())
    }

    #[test]
    fn last_good_skips_failures_and_the_failed_revision() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let start = |action: &str| -> AppResult<DeployRun> {
            Ok(DeployRun::after_revision(
                &conn,
                &revision(&conn, "site", action)?,
                "web",
                DEFAULT_ROLLOUT_DEADLINE_MS,
            )?
            .expect("a deploy starts a run"))
        };
        let good = start("deploy")?;
        DeployRun::finish(&conn, good.id, RunState::Succeeded, None)?;
        let bad = start("deploy")?;
        DeployRun::finish(&conn, bad.id, RunState::Failed, Some("crash"))?;
        let failing = start("deploy")?;

        let target = DeployRun::last_good(&conn, "site", failing.id, failing.revision_id)?;
        assert_eq!(target.map(|r| r.id), Some(good.id));
        // The good run's own revision is excluded when it is the one that
        // failed (a redeploy of it).
        assert!(DeployRun::last_good(&conn, "site", failing.id, good.revision_id)?.is_none());
        assert!(DeployRun::last_good(&conn, "site", good.id, None)?.is_none());

        DeployRun::mark_rolled_back(&conn, bad.id)?;
        DeployRun::mark_rolled_back(&conn, good.id)?;
        assert_eq!(
            DeployRun::get(&conn, bad.id)?.map(|r| r.state),
            Some(RunState::RolledBack)
        );
        // Only a failed run can be marked rolled back.
        assert_eq!(
            DeployRun::get(&conn, good.id)?.map(|r| r.state),
            Some(RunState::Succeeded)
        );
        Ok(())
    }

    #[test]
    fn runs_are_found_by_revision() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let deployed = revision(&conn, "site", "deploy")?;
        let undeployed = revision(&conn, "site", "undeploy")?;
        let run = DeployRun::after_revision(&conn, &deployed, "web", DEFAULT_ROLLOUT_DEADLINE_MS)?
            .expect("a deploy starts a run");
        DeployRun::after_revision(&conn, &undeployed, "web", DEFAULT_ROLLOUT_DEADLINE_MS)?;
        let runs = DeployRun::for_revisions(&conn, &[deployed.id, undeployed.id, 999])?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs.get(&deployed.id).map(|r| r.id), Some(run.id));
        assert!(DeployRun::for_revisions(&conn, &[])?.is_empty());
        Ok(())
    }

    #[test]
    fn a_waiting_run_continues_as_the_deploy_that_goes_ahead() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let waiting = DeployRun::wait(&conn, "site", "autodeploy", "streams: busy", 100)?;
        assert_eq!(waiting.state, RunState::Waiting);
        DeployRun::keep_waiting(&conn, waiting.id, "streams: still busy", 200)?;
        assert_eq!(
            DeployRun::get(&conn, waiting.id)?.and_then(|r| r.next_check_at),
            Some(200)
        );

        let rev = revision(&conn, "site", "deploy")?;
        let run = DeployRun::after_revision_with(
            &conn,
            &rev,
            "autodeploy",
            DEFAULT_ROLLOUT_DEADLINE_MS,
            Some(waiting.id),
            &["streams".to_string()],
        )?
        .expect("a deploy starts a run");
        assert_eq!(run.id, waiting.id, "the waiting run carries on");
        assert_eq!(run.state, RunState::RollingOut);
        assert_eq!(run.revision_id, Some(rev.id));
        assert_eq!(run.message, None);
        assert_eq!(run.next_check_at, None);
        assert_eq!(run.acknowledged, vec!["streams".to_string()]);
        Ok(())
    }

    #[test]
    fn a_deploy_supersedes_a_waiting_run_and_cancel_drops_one() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let waiting = DeployRun::wait(&conn, "site", "autodeploy", "busy", 100)?;
        let manual = DeployRun::after_revision(
            &conn,
            &revision(&conn, "site", "deploy")?,
            "web",
            DEFAULT_ROLLOUT_DEADLINE_MS,
        )?
        .expect("a deploy starts a run");
        let waiting = DeployRun::get(&conn, waiting.id)?.expect("exists");
        assert_eq!(waiting.state, RunState::Superseded);
        assert_eq!(waiting.superseded_by, Some(manual.id));

        let other = DeployRun::wait(&conn, "site", "autodeploy", "busy", 100)?;
        DeployRun::cancel_waiting(&conn, other.id, "autodeploy was turned off")?;
        let other = DeployRun::get(&conn, other.id)?.expect("exists");
        assert_eq!(other.state, RunState::Superseded);
        assert_eq!(other.message.as_deref(), Some("autodeploy was turned off"));
        assert!(DeployRun::waiting(&conn)?.is_empty());
        Ok(())
    }

    #[test]
    fn migration_seeds_a_succeeded_run_from_each_deployed_config() -> AppResult<()> {
        use crate::db::migrations::migrate;
        use crate::db::test_support::memory_pool_at_version;
        let pool = memory_pool_at_version(7);
        let conn = pool.get()?;
        let insert = "INSERT INTO revision (config_name, created_at, actor, action, config_sha) VALUES (?1, ?2, 'web', ?3, ?4)";
        conn.execute(insert, params!["site", 1000, "deploy", Some("c1")])?;
        conn.execute(insert, params!["site", 2000, "patch", Some("c1")])?;
        conn.execute(insert, params!["gone", 1500, "deploy", Some("c1")])?;
        conn.execute(insert, params!["gone", 2500, "undeploy", None::<String>])?;
        drop(conn);

        migrate(pool.get()?)?;
        let conn = pool.get()?;
        let site = DeployRun::list_for(&conn, "site", 10)?;
        assert_eq!(site.len(), 1);
        assert_eq!(site[0].state, RunState::Succeeded);
        assert_eq!(site[0].trigger, "backfill");
        assert_eq!(site[0].created_at, 2000);
        let revision = Revision::get(&conn, site[0].revision_id.expect("seeded from a revision"))?
            .expect("revision exists");
        assert_eq!(revision.action, "patch");
        assert!(DeployRun::list_for(&conn, "gone", 10)?.is_empty());
        Ok(())
    }
}
