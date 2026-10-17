//! What each deploy check said for a run, kept so history can show why a
//! deploy waited or what a person acknowledged.
//!
//! A run keeps the latest answer of each check per phase: a held
//! autodeploy that asks again replaces its pre-check rows, and `attempt`
//! counts the asks.
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Row};

use crate::checks::PreOutcome;
use crate::error::AppResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub run_id: i64,
    /// `pre` (post-checks come later).
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
}

const COLUMNS: &str =
    "run_id, phase, check_name, status, title, message, details_url, attempt, started_at, finished_at";

impl CheckResult {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(CheckResult {
            run_id: row.get(0)?,
            phase: row.get(1)?,
            check_name: row.get(2)?,
            status: row.get(3)?,
            title: row.get(4)?,
            message: row.get(5)?,
            details_url: row.get(6)?,
            attempt: row.get(7)?,
            started_at: row.get(8)?,
            finished_at: row.get(9)?,
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
                &format!("INSERT INTO check_result ({COLUMNS}) VALUES (?1, 'pre', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
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
