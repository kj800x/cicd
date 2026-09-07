//! Helpers for tests that need a real sqlite schema.

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::migrations::{migrate, migrations};

/// An in-memory database with every migration applied.
///
/// The pool is capped at one connection on purpose: each `:memory:`
/// connection is its own database, so a larger pool would hand later
/// callers an empty, unmigrated one.
pub fn migrated_memory_pool() -> Pool<SqliteConnectionManager> {
    let pool = memory_pool();
    #[allow(clippy::expect_used)]
    let conn = pool.get().expect("connection from in-memory pool");
    #[allow(clippy::expect_used)]
    migrate(conn).expect("migrations apply cleanly to an empty database");
    pool
}

/// An in-memory database migrated only up to `version`, for tests that seed
/// data an older schema would have held and then run the later migrations.
pub fn memory_pool_at_version(version: usize) -> Pool<SqliteConnectionManager> {
    let pool = memory_pool();
    #[allow(clippy::expect_used)]
    let mut conn = pool.get().expect("connection from in-memory pool");
    #[allow(clippy::expect_used)]
    migrations()
        .to_version(&mut conn, version)
        .expect("migrations apply up to the requested version");
    pool
}

fn memory_pool() -> Pool<SqliteConnectionManager> {
    #[allow(clippy::expect_used)]
    Pool::builder()
        .max_size(1)
        .build(SqliteConnectionManager::memory())
        .expect("in-memory sqlite pool")
}
