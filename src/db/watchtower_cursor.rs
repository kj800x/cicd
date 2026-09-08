//! Where cicd is in watchtower's event feed: the id of the last event it
//! handled. One row; absent until the poller first runs.

use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

use crate::error::AppResult;

pub fn get(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Option<u64>> {
    Ok(conn
        .query_row(
            "SELECT after FROM watchtower_cursor WHERE id = 1",
            params![],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|v| v as u64))
}

pub fn set(conn: &PooledConnection<SqliteConnectionManager>, after: u64) -> AppResult<()> {
    conn.execute(
        "INSERT INTO watchtower_cursor (id, after) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET after = excluded.after",
        params![after as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::migrated_memory_pool;

    #[test]
    fn cursor_is_absent_then_sticks() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        assert_eq!(get(&conn)?, None);
        set(&conn, 41)?;
        set(&conn, 42)?;
        assert_eq!(get(&conn)?, Some(42));
        Ok(())
    }
}
