use crate::{
    crab_ext::IRepo,
    error::{AppError, AppResult},
    webhooks::models::Repository as WebhookRepository,
};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
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

/// From the REST API's repository model, as returned by `GET /repos/{owner}/{repo}`
/// and the list endpoints. Fails only if GitHub omitted the owner, which it
/// does not do for real repositories.
impl TryFrom<octocrab::models::Repository> for GitRepo {
    type Error = AppError;

    fn try_from(repo: octocrab::models::Repository) -> AppResult<Self> {
        let owner_name = repo
            .owner
            .as_ref()
            .map(|owner| owner.login.clone())
            .ok_or_else(|| {
                AppError::Parse(format!("GitHub repository {} has no owner", repo.name))
            })?;

        Ok(Self {
            id: repo.id.0,
            owner_name,
            name: repo.name,
            default_branch: repo.default_branch.unwrap_or_else(|| "main".to_string()),
            private: repo.private.unwrap_or(false),
            language: repo
                .language
                .as_ref()
                .and_then(|v| v.as_str().map(|s| s.to_string())),
        })
    }
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

    pub fn get(
        repository: impl IRepo,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        Self::get_by_name(repository.owner(), repository.repo(), conn)
    }

    pub fn get_by_name(
        owner_name: &str,
        name: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let repo = conn.prepare("SELECT id, owner_name, name, default_branch, private, language FROM git_repo WHERE owner_name = ?1 AND name = ?2")?
          .query_row(params![owner_name, name], |row| {
            Ok(GitRepo::from_row(row))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(repo)
    }

    pub fn get_by_id(
        id: &u64,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let repo = conn.prepare("SELECT id, owner_name, name, default_branch, private, language FROM git_repo WHERE id = ?1")?
          .query_row(params![id], |row| {
            Ok(GitRepo::from_row(row))
          })
          .optional().map_err(AppError::from)?.transpose()?;

        Ok(repo)
    }

    pub fn upsert(&self, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("INSERT OR REPLACE INTO git_repo (id, owner_name, name, default_branch, private, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
          .execute(params![self.id, self.owner_name, self.name, self.default_branch, self.private, self.language])?;

        Ok(())
    }

    pub fn get_all(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<Vec<Self>> {
        let mut repos = Vec::new();
        let mut stmt = conn.prepare("SELECT id, owner_name, name, default_branch, private, language FROM git_repo ORDER BY owner_name, name")?;
        let mut rows = stmt.query([])?;

        while let Some(row) = rows.next()? {
            repos.push(GitRepo::from_row(row)?);
        }

        Ok(repos)
    }
}
