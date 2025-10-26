use crate::{crab_ext::IRepo, error::AppResult, webhooks::models::Repository as WebhookRepository};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct GitRepo {
    pub id: u64,
    pub owner_name: String,
    pub name: String,
    pub default_branch: String,
    pub private: bool,
    pub language: Option<String>,
}

impl IRepo for GitRepo {
    fn owner(&self) -> &str {
        &self.owner_name
    }
    fn repo(&self) -> &str {
        &self.name
    }
}

impl From<WebhookRepository> for GitRepo {
    fn from(repo: WebhookRepository) -> Self {
        Self {
            id: repo.id,
            owner_name: repo.owner.login,
            name: repo.name,
            default_branch: repo.default_branch,
            private: repo.private,
            language: repo.language,
        }
    }
}

pub struct GitRepoEgg {
    pub owner_name: String,
    pub name: String,
    pub default_branch: String,
    pub private: bool,
    pub language: Option<String>,
}

impl GitRepo {
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(GitRepo {
            id: row.get(0)?,
            owner_name: row.get(1)?,
            name: row.get(2)?,
            default_branch: row.get(3)?,
            private: row.get(4)?,
            language: row.get(5)?,
        })
    }

    pub fn from_egg(egg: &GitRepoEgg, id: u64) -> Self {
        Self {
            id,
            owner_name: egg.owner_name.clone(),
            name: egg.name.clone(),
            default_branch: egg.default_branch.clone(),
            private: egg.private,
            language: egg.language.clone(),
        }
    }

    pub fn upsert(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("INSERT OR REPLACE INTO git_repo (id, owner_name, name, default_branch, private, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
          .execute(params![self.id, self.owner_name, self.name, self.default_branch, self.private, self.language])?;

        Ok(())
    }
}
