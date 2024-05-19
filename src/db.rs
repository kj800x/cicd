use crate::prelude::*;
use rusqlite::ffi::Error;
use serde_variant::to_variant_name;

#[derive(Debug, Serialize, Deserialize)]
pub enum BuildStatus {
    None,
    Pending,
    Success,
    Failure,
}

impl From<String> for BuildStatus {
    fn from(s: String) -> Self {
        match s.as_str() {
            "None" => BuildStatus::None,
            "Pending" => BuildStatus::Pending,
            "Success" => BuildStatus::Success,
            "Failure" => BuildStatus::Failure,
            _ => BuildStatus::None,
        }
    }
}

impl BuildStatus {
    pub fn of(status: &str, conclusion: &Option<&str>) -> Self {
        match status {
            "queued" => BuildStatus::Pending,
            "completed" => match conclusion {
                Some("success") => BuildStatus::Success,
                Some("failure") => BuildStatus::Failure,
                _ => BuildStatus::None,
            },
            _ => BuildStatus::None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub owner_name: String,
    pub default_branch: String,
    pub private: bool,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Branch {
    pub id: i64,
    pub name: String,
    pub head_commit_sha: String,
    pub repo_id: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Commit {
    pub id: i64,
    pub sha: String,
    pub message: String,
    pub timestamp: i64,
    pub build_status: BuildStatus,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommitWithRepo {
    pub commit: Commit,
    pub repo: Repo,
}

pub fn migrate(mut conn: PooledConnection<SqliteConnectionManager>) -> Result<(), rusqlite::Error> {
    let migrations: Migrations = Migrations::new(vec![
        M::up(
            "CREATE TABLE git_repo (
                id INTEGER PRIMARY KEY NOT NULL,
                owner_name TEXT NOT NULL,
                name TEXT NOT NULL,
                default_branch TEXT NOT NULL,
                private BOOLEAN NOT NULL,
                language TEXT
            );

            CREATE TABLE git_branch (
                id INTEGER PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                head_commit_sha TEXT NOT NULL,
                repo_id INTEGER NOT NULL,
                FOREIGN KEY(repo_id) REFERENCES repo(id)
            );

            CREATE TABLE git_commit (
                id INTEGER PRIMARY KEY NOT NULL,
                sha TEXT NOT NULL,
                message TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                build_status TEXT,
                repo_id INTEGER NOT NULL,
                FOREIGN KEY(repo_id) REFERENCES repo(id)
            );

            CREATE INDEX idx_git_repo_owner_name ON git_repo(owner_name, name);
            CREATE INDEX idx_git_branch_repo_id ON git_branch(repo_id);
            CREATE INDEX idx_git_branch_repo_name ON git_branch(repo_id, name);
            CREATE INDEX idx_git_commit_sha ON git_commit(sha);
            ",
        ),
        // In the future, add more migrations here:
        //M::up("ALTER TABLE friend ADD COLUMN email TEXT;"),
    ]);

    conn.pragma_update_and_check(None, "journal_mode", &"WAL", |_| Ok(()))
        .unwrap();
    migrations.to_latest(&mut conn).unwrap();
    Ok(())
}

struct ExistenceResult {
    id: u64,
}

pub fn upsert_repo(
    repo: &crate::webhooks::Repository,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<u64, Error> {
    let repo = repo.clone();

    let existing = conn
        .prepare("SELECT id FROM git_repo WHERE id = ?1")
        .unwrap()
        .query_row([repo.id], |row| Ok(ExistenceResult { id: row.get(0)? }))
        .optional()
        .unwrap();

    match existing {
        Some(_) => {
            conn.prepare(
                    "UPDATE git_repo SET name = ?2, owner_name = ?3, default_branch = ?4, private = ?5, language = ?6 WHERE id = ?1",
                ).unwrap()
                .execute(params![
                    repo.id,
                    repo.name,
                    repo.owner.login,
                    repo.default_branch,
                    repo.private,
                    repo.language,
                ]).unwrap();

            Ok(repo.id)
        }
        None => {
            conn.prepare(
                    "INSERT INTO git_repo (id, name, owner_name, default_branch, private, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                ).unwrap()
                .execute(params![
                    repo.id,
                    repo.name,
                    repo.owner.login,
                    repo.default_branch,
                    repo.private,
                    repo.language,
                ]).unwrap();

            Ok(repo.id)
        }
    }
}

pub fn upsert_commit(
    commit: &crate::webhooks::GhCommit,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<u64, Error> {
    let commit = commit.clone();

    let existing = conn
        .prepare("SELECT id FROM git_commit WHERE sha = ?1 AND repo_id = ?2")
        .unwrap()
        .query_row(params![commit.id, repo_id], |row| {
            Ok(ExistenceResult { id: row.get(0)? })
        })
        .optional()
        .unwrap();

    match existing {
        Some(ExistenceResult { id }) => {
            conn.prepare(
                    "UPDATE git_commit SET message = ?2, timestamp = ?3 WHERE sha = ?1 AND repo_id = ?4",
                ).unwrap().execute(params![
                    commit.id,
                    commit.message,
                    DateTime::parse_from_rfc3339(&commit.timestamp).unwrap().timestamp_millis(),
                    repo_id,
                ]).unwrap();

            Ok(id)
        }
        None => {
            conn.prepare(
                "INSERT INTO git_commit (sha, message, timestamp, repo_id) VALUES (?1, ?2, ?3, ?4)",
            )
            .unwrap()
            .execute(params![
                commit.id,
                commit.message,
                DateTime::parse_from_rfc3339(&commit.timestamp)
                    .unwrap()
                    .timestamp_millis(),
                repo_id,
            ])
            .unwrap();

            Ok(conn.last_insert_rowid() as u64)
        }
    }
}

pub fn upsert_branch(
    name: &str,
    sha: &str,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<u64, Error> {
    let name = name.to_string();
    let sha = sha.to_string();

    let existing = conn
        .prepare("SELECT id FROM git_branch WHERE name = ?1 AND repo_id = ?2")
        .unwrap()
        .query_row(params![name, repo_id], |row| {
            Ok(ExistenceResult { id: row.get(0)? })
        })
        .optional()
        .unwrap();

    match existing {
        Some(ExistenceResult { id }) => {
            conn.prepare(
                "UPDATE git_branch SET head_commit_sha = ?3 WHERE name = ?1 AND repo_id = ?2",
            )
            .unwrap()
            .execute(params![name, repo_id, sha])
            .unwrap();

            Ok(id)
        }
        None => {
            conn.prepare(
                "INSERT INTO git_branch (name, repo_id, head_commit_sha) VALUES (?1, ?2, ?3)",
            )
            .unwrap()
            .execute(params![name, repo_id, sha])
            .unwrap();

            Ok(conn.last_insert_rowid() as u64)
        }
    }
}

pub fn set_commit_status(
    sha: &str,
    build_status: BuildStatus,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), Error> {
    let sha = sha.to_string();

    conn.prepare("UPDATE git_commit SET build_status = ?1 WHERE sha = ?2 AND repo_id = ?3")
        .unwrap()
        .execute(params![
            to_variant_name(&build_status).unwrap(),
            sha,
            repo_id
        ])
        .unwrap();

    Ok(())
}

pub fn get_repo(
    conn: &PooledConnection<SqliteConnectionManager>,
    owner_name: String,
    repo_name: String,
) -> Result<Option<Repo>, Error> {
    Ok(conn.prepare(
        "SELECT id, name, owner_name, default_branch, private, language FROM git_repo WHERE owner_name = ?1 AND name = ?2 LIMIT 1",
    )
    .unwrap()
    .query_row([owner_name, repo_name], |row| {
        Ok(Repo {
            id: row.get(0)?,
            name: row.get(1)?,
            owner_name: row.get(2)?,
            default_branch: row.get(3)?,
            private: row.get(4)?,
            language: row.get(5)?,
        })
    })
    .optional()
    .unwrap())
}

pub fn get_branch(
    conn: &PooledConnection<SqliteConnectionManager>,
    repo_id: i64,
    branch_name: String,
) -> Result<Option<Branch>, Error> {
    Ok(conn.prepare(
        "SELECT id, name, head_commit_sha, repo_id FROM git_branch WHERE name = ?1 AND repo_id = ?2 LIMIT 1",
    )
    .unwrap()
    .query_row(params![branch_name, repo_id], |row| {
        Ok(Branch {
            id: row.get(0)?,
            name: row.get(1)?,
            head_commit_sha: row.get(2)?,
            repo_id: row.get(3)?,
        })
    })
    .optional().unwrap())
}

pub fn get_commit(
    conn: &PooledConnection<SqliteConnectionManager>,
    repo_id: i64,
    commit_sha: String,
) -> Result<Option<Commit>, Error> {
    Ok(conn.prepare(
        "SELECT id, sha, message, timestamp, build_status, repo_id FROM git_commit WHERE sha = ?1 AND repo_id = ?2 LIMIT 1",
    ).unwrap()
    .query_row(params![commit_sha, repo_id], |row| {
        Ok(Commit {
            id: row.get(0)?,
            sha: row.get(1)?,
            message: row.get(2)?,
            timestamp: row.get(3)?,
            build_status: row.get::<usize, String>(4)?.into(),
        })
    })
    .optional().unwrap())
}

pub fn get_commits_since(
    conn: &PooledConnection<SqliteConnectionManager>,
    since: i64,
) -> Result<Vec<CommitWithRepo>, Error> {
    let mut stmt = conn.prepare(
            "SELECT id, sha, message, timestamp, build_status, repo_id FROM git_commit WHERE timestamp > ?1 ORDER BY timestamp DESC",
        ).unwrap();
    let mut rows = stmt.query(params![since]).unwrap();
    let mut commits = Vec::new();

    while let Some(row) = rows.next().unwrap() {
        let repo_id = row.get::<usize, i64>(5).unwrap();

        let repo = conn.prepare(
                "SELECT id, name, owner_name, default_branch, private, language FROM git_repo WHERE id = ?1 LIMIT 1",
            ).unwrap()
            .query_row(params![repo_id], |row| {
                Ok(Repo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    owner_name: row.get(2)?,
                    default_branch: row.get(3)?,
                    private: row.get(4)?,
                    language: row.get(5)?,
                })
            })
            .optional().unwrap().unwrap();

        commits.push(CommitWithRepo {
            repo,
            commit: Commit {
                id: row.get(0).unwrap(),
                sha: row.get(1).unwrap(),
                message: row.get(2).unwrap(),
                timestamp: row.get(3).unwrap(),
                build_status: row.get::<usize, String>(4).unwrap().into(),
            },
        });
    }

    Ok(commits)
}
