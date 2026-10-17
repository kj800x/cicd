//! What each deploy check said for a run, kept so history can show why a
//! deploy waited or what a person acknowledged.
//!
//! A run keeps the latest answer of each check per phase: a held
//! autodeploy that asks again replaces its pre-check rows, and `attempt`
//! counts the asks. Post-check rows start `PENDING` when a run starts
//! monitoring and are updated in place as the driver polls them, until
//! each has a final answer or reaches its deadline.
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Row};

use crate::checks::PreOutcome;
use crate::error::AppResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub id: i64,
    pub run_id: i64,
    /// `pre` or `post`.
    pub phase: String,
    pub check_name: String,
    /// The protocol status, e.g. `SOFT_BLOCK`.
    pub status: String,
    pub title: Option<String>,
    pub message: Option<String>,
    pub details_url: Option<String>,
    pub attempt: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Post-checks: when to ask again while `PENDING`.
    pub next_poll_at: Option<i64>,
    /// Post-checks: when a `PENDING` check gives up.
    pub deadline_at: Option<i64>,
    /// Post-checks: what the check started, e.g. a jobfather job's name.
    pub handle: Option<String>,
}

const COLUMNS: &str = "id, run_id, phase, check_name, status, title, message, details_url, attempt, started_at, finished_at, next_poll_at, deadline_at, handle";
const INSERT_COLUMNS: &str =
    "run_id, phase, check_name, status, title, message, details_url, attempt, started_at, finished_at";

impl CheckResult {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(CheckResult {
            id: row.get(0)?,
            run_id: row.get(1)?,
            phase: row.get(2)?,
            check_name: row.get(3)?,
            status: row.get(4)?,
            title: row.get(5)?,
            message: row.get(6)?,
            details_url: row.get(7)?,
            attempt: row.get(8)?,
            started_at: row.get(9)?,
            finished_at: row.get(10)?,
            next_poll_at: row.get(11)?,
            deadline_at: row.get(12)?,
            handle: row.get(13)?,
        })
    }

    /// Replace a run's pre-check rows with `outcomes`, the `attempt`th ask.
    pub fn record_pre(
        conn: &PooledConnection<SqliteConnectionManager>,
        run_id: i64,
        outcomes: &[PreOutcome],
        attempt: i64,
    ) -> AppResult<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM check_result WHERE run_id = ?1 AND phase = 'pre'",
            params![run_id],
        )?;
        for o in outcomes {
            tx.execute(
                &format!("INSERT INTO check_result ({INSERT_COLUMNS}) VALUES (?1, 'pre', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
                params![
                    run_id,
                    o.name,
                    o.status.as_str(),
                    o.title,
                    o.message,
                    o.details_url,
                    attempt,
                    o.started_at,
                    o.finished_at
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Start a run's post-checks: one `PENDING` row per check, due now,
    /// each giving up at `now` plus its deadline in milliseconds.
    pub fn start_post(
        conn: &PooledConnection<SqliteConnectionManager>,
        run_id: i64,
        checks: &[(String, i64)],
        now: i64,
    ) -> AppResult<()> {
        let tx = conn.unchecked_transaction()?;
        for (name, deadline_ms) in checks {
            tx.execute(
                "INSERT INTO check_result (run_id, phase, check_name, status, attempt, started_at, next_poll_at, deadline_at) \
                 VALUES (?1, 'post', ?2, 'PENDING', 0, ?3, ?3, ?4)",
                params![run_id, name, now, now + deadline_ms],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A run's post-check rows, in config order.
    pub fn post_for_run(
        conn: &PooledConnection<SqliteConnectionManager>,
        run_id: i64,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM check_result WHERE run_id = ?1 AND phase = 'post' ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![run_id], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Write back a polled post-check.
    pub fn save(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.execute(
            "UPDATE check_result SET status = ?1, title = ?2, message = ?3, details_url = ?4, attempt = ?5, \
             finished_at = ?6, next_poll_at = ?7, handle = ?8 WHERE id = ?9",
            params![
                self.status,
                self.title,
                self.message,
                self.details_url,
                self.attempt,
                self.finished_at,
                self.next_poll_at,
                self.handle,
                self.id
            ],
        )?;
        Ok(())
    }

    /// Every result of a run, pre-checks first, in the order they were
    /// recorded.
    pub fn for_run(
        conn: &PooledConnection<SqliteConnectionManager>,
        run_id: i64,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM check_result WHERE run_id = ?1 ORDER BY phase DESC, id"
        ))?;
        let rows = stmt.query_map(params![run_id], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::PreStatus;
    use crate::db::test_support::migrated_memory_pool;

    #[test]
    fn post_checks_start_pending_and_are_saved_in_place() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO deploy_run (id, config_name, trigger, state, created_at) VALUES (1, 'site', 'web', 'MONITORING', 0)",
            [],
        )?;
        CheckResult::start_post(
            &conn,
            1,
            &[("smoke".into(), 600_000), ("quiet".into(), 60_000)],
            1000,
        )?;
        let mut rows = CheckResult::post_for_run(&conn, 1)?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].check_name, "smoke");
        assert_eq!(rows[0].status, "PENDING");
        assert_eq!(rows[0].next_poll_at, Some(1000));
        assert_eq!(rows[1].deadline_at, Some(61_000));

        let row = &mut rows[0];
        row.status = "OK".into();
        row.handle = Some("smoke-abc".into());
        row.attempt = 3;
        row.finished_at = Some(2000);
        row.save(&conn)?;
        let rows = CheckResult::post_for_run(&conn, 1)?;
        assert_eq!(rows[0].status, "OK");
        assert_eq!(rows[0].handle.as_deref(), Some("smoke-abc"));
        assert_eq!(rows[0].attempt, 3);
        // Pre-check rows are untouched by post-checks and listed first.
        assert_eq!(CheckResult::for_run(&conn, 1)?.len(), 2);
        Ok(())
    }

    #[test]
    fn a_new_ask_replaces_the_last() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO deploy_run (id, config_name, trigger, state, created_at) VALUES (1, 'site', 'autodeploy', 'WAITING', 0)",
            [],
        )?;
        let outcome = |status, title: &str| PreOutcome {
            name: "streams".into(),
            status,
            title: Some(title.into()),
            message: None,
            details_url: None,
            started_at: 10,
            finished_at: 20,
        };
        CheckResult::record_pre(
            &conn,
            1,
            &[outcome(PreStatus::SoftBlock, "2 active streams")],
            1,
        )?;
        CheckResult::record_pre(&conn, 1, &[outcome(PreStatus::Ok, "idle")], 2)?;
        let results = CheckResult::for_run(&conn, 1)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "OK");
        assert_eq!(results[0].title.as_deref(), Some("idle"));
        assert_eq!(results[0].attempt, 2);
        assert_eq!(results[0].finished_at, Some(20));
        Ok(())
    }
}
