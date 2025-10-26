use crate::error::AppResult;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct AutodeployState {
    pub name: String,
    pub enabled: bool,
}

impl AutodeployState {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(AutodeployState {
            name: row.get(0)?,
            enabled: row.get(1)?,
        })
    }

    pub fn insert(
        state: &AutodeployState,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        conn.prepare("INSERT INTO autodeploy_state (name, enabled) VALUES (?1, ?2)")?
            .execute(params![state.name, state.enabled])?;

        Ok(Self {
            name: state.name.clone(),
            enabled: state.enabled,
        })
    }

    pub fn update(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("UPDATE autodeploy_state SET enabled = ?2 WHERE name = ?1")?
            .execute(params![self.name, self.enabled])?;

        Ok(())
    }
}
