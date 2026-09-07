//! Blockers: a per-config hold with a reason.
//!
//! While a config has an active blocker, manual deploys are refused and
//! autodeploy is suspended. Undeploy, bounce and job execution are not
//! affected. Blockers are per config, never per parameter: "do not upgrade
//! this one thing" is expressed by pinning the parameter instead.
//!
//! Rows are never deleted. Clearing records who cleared it and when, so a
//! config's hold history stays readable.
use chrono::Utc;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension, Row};

use crate::error::{AppError, AppResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub id: i64,
    pub config_name: String,
    pub reason: String,
    pub created_by: String,
    /// Milliseconds since the epoch, like `deploy_event.timestamp`.
    pub created_at: i64,
    pub cleared_by: Option<String>,
    pub cleared_at: Option<i64>,
}

const COLUMNS: &str = "id, config_name, reason, created_by, created_at, cleared_by, cleared_at";

#[allow(dead_code)] // the UI and MCP surfaces that manage blockers land in the next PRs
impl Blocker {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Blocker {
            id: row.get(0)?,
            config_name: row.get(1)?,
            reason: row.get(2)?,
            created_by: row.get(3)?,
            created_at: row.get(4)?,
            cleared_by: row.get(5)?,
            cleared_at: row.get(6)?,
        })
    }

    pub fn is_active(&self) -> bool {
        self.cleared_at.is_none()
    }

    /// Add a blocker. The reason is required: a hold nobody can explain is
    /// one nobody dares to clear.
    pub fn create(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
        reason: &str,
        created_by: &str,
    ) -> AppResult<Self> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(AppError::InvalidInput(
                "A blocker needs a reason".to_string(),
            ));
        }
        let created_by = created_by.trim();
        let created_by = if created_by.is_empty() {
            "unknown"
        } else {
            created_by
        };
        let created_at = Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO blocker (config_name, reason, created_by, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![config_name, reason, created_by, created_at],
        )?;
        Ok(Blocker {
            id: conn.last_insert_rowid(),
            config_name: config_name.to_string(),
            reason: reason.to_string(),
            created_by: created_by.to_string(),
            created_at,
            cleared_by: None,
            cleared_at: None,
        })
    }

    pub fn get(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
    ) -> AppResult<Option<Self>> {
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM blocker WHERE id = ?1"),
                params![id],
                Self::from_row,
            )
            .optional()?)
    }

    /// Active blockers for one config, oldest first.
    pub fn active_for(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM blocker WHERE config_name = ?1 AND cleared_at IS NULL ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(params![config_name], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every active blocker across all configs, oldest first.
    pub fn all_active(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM blocker WHERE cleared_at IS NULL ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map([], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full history for one config, newest first.
    pub fn history_for(
        conn: &PooledConnection<SqliteConnectionManager>,
        config_name: &str,
        limit: usize,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM blocker WHERE config_name = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![config_name, limit as i64], Self::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Clear a blocker. Returns `false` if it does not exist or was already
    /// cleared, so a double click is not an error.
    pub fn clear(
        conn: &PooledConnection<SqliteConnectionManager>,
        id: i64,
        cleared_by: &str,
    ) -> AppResult<bool> {
        let cleared_by = cleared_by.trim();
        let cleared_by = if cleared_by.is_empty() {
            "unknown"
        } else {
            cleared_by
        };
        let changed = conn.execute(
            "UPDATE blocker SET cleared_by = ?1, cleared_at = ?2 WHERE id = ?3 AND cleared_at IS NULL",
            params![cleared_by, Utc::now().timestamp_millis(), id],
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::migrated_memory_pool;

    #[test]
    fn create_list_and_clear() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;

        let b = Blocker::create(&conn, "site", "  incident 42  ", "kevin")?;
        assert_eq!(b.reason, "incident 42");
        assert!(b.is_active());
        assert_eq!(Blocker::active_for(&conn, "site")?, vec![b.clone()]);
        assert!(Blocker::active_for(&conn, "other")?.is_empty());
        assert_eq!(Blocker::all_active(&conn)?.len(), 1);

        assert!(Blocker::clear(&conn, b.id, "kevin")?);
        assert!(
            !Blocker::clear(&conn, b.id, "kevin")?,
            "second clear is a no-op"
        );
        assert!(Blocker::active_for(&conn, "site")?.is_empty());

        let stored = Blocker::get(&conn, b.id)?.ok_or(AppError::NotFound("blocker".into()))?;
        assert!(!stored.is_active());
        assert_eq!(stored.cleared_by.as_deref(), Some("kevin"));
        assert_eq!(Blocker::history_for(&conn, "site", 10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn several_blockers_stack_and_clear_independently() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        let incident = Blocker::create(&conn, "site", "incident", "a")?;
        let freeze = Blocker::create(&conn, "site", "schema freeze", "b")?;
        assert_eq!(Blocker::active_for(&conn, "site")?.len(), 2);
        assert!(Blocker::clear(&conn, incident.id, "a")?);
        let left = Blocker::active_for(&conn, "site")?;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, freeze.id);
        Ok(())
    }

    #[test]
    fn reason_is_required_and_creator_defaults() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        assert!(matches!(
            Blocker::create(&conn, "site", "   ", "x"),
            Err(AppError::InvalidInput(_))
        ));
        let b = Blocker::create(&conn, "site", "why", "")?;
        assert_eq!(b.created_by, "unknown");
        assert!(Blocker::get(&conn, 9999)?.is_none());
        assert!(!Blocker::clear(&conn, 9999, "x")?);
        Ok(())
    }
}
