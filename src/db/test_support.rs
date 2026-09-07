//! Helpers for tests that need a real sqlite schema.

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::migrations::migrate;

/// An in-memory database with every migration applied.
///
/// The pool is capped at one connection on purpose: each `:memory:`
/// connection is its own database, so a larger pool would hand later
/// callers an empty, unmigrated one.
pub fn migrated_memory_pool() -> Pool<SqliteConnectionManager> {
    #[allow(clippy::expect_used)]
    let pool = Pool::builder()
        .max_size(1)
        .build(SqliteConnectionManager::memory())
        .expect("in-memory sqlite pool");
    #[allow(clippy::expect_used)]
    let conn = pool.get().expect("connection from in-memory pool");
    #[allow(clippy::expect_used)]
    migrate(conn).expect("migrations apply cleanly to an empty database");
    pool
}
