use rusqlite::ffi::Error;

use crate::prelude::*;
use serde_variant::to_variant_name;

#[derive(Debug, Serialize, Deserialize)]
pub enum BuildStatus {
    None,
    Pending,
    Success,
    Failure,
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

pub fn migrate(pool: &Pool<SqliteConnectionManager>) -> Result<(), rusqlite::Error> {
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

    let mut conn = pool.get().unwrap();
    conn.pragma_update_and_check(None, "journal_mode", &"WAL", |_| Ok(()))
        .unwrap();
    migrations.to_latest(&mut conn).unwrap();
    Ok(())
}

struct ExistenceResult {
    id: u64,
}

pub async fn upsert_repo(
    repo: &crate::webhooks::Repository,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<u64, Error> {
    let pool = pool.clone();
    let repo = repo.clone();
    let conn = web::block(move || pool.get())
        .await
        .unwrap()
        .map_err(error::ErrorInternalServerError)
        .unwrap();

    web::block(move || {
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
            },
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
            },
        }
    })
    .await.unwrap()
    // .map_err(error::ErrorInternalServerError)
}

pub async fn upsert_commit(
    commit: &crate::webhooks::Commit,
    repo_id: u64,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<u64, Error> {
    let pool = pool.clone();
    let commit = commit.clone();
    let conn = web::block(move || pool.get())
        .await
        .unwrap()
        .map_err(error::ErrorInternalServerError)
        .unwrap();

    web::block(move || {
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
                conn.prepare("INSERT INTO git_commit (sha, message, timestamp, repo_id) VALUES (?1, ?2, ?3, ?4)")
                    .unwrap()
                    .execute(params![
                        commit.id,
                        commit.message,
                        DateTime::parse_from_rfc3339(&commit.timestamp).unwrap().timestamp_millis(),
                        repo_id,
                    ])
                    .unwrap();

                Ok(conn.last_insert_rowid() as u64)
            }
        }
    })
    .await
    .unwrap()
    // .map_err(error::ErrorInternalServerError)
}

pub async fn upsert_branch(
    name: &str,
    sha: &str,
    repo_id: u64,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<u64, Error> {
    let pool = pool.clone();
    let name = name.to_string();
    let sha = sha.to_string();
    let conn = web::block(move || pool.get())
        .await
        .unwrap()
        .map_err(error::ErrorInternalServerError)
        .unwrap();

    web::block(move || {
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
    })
    .await
    .unwrap()
    // .map_err(error::ErrorInternalServerError)
}

pub async fn set_commit_status(
    sha: &str,
    build_status: BuildStatus,
    repo_id: u64,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<(), Error> {
    let pool = pool.clone();
    let sha = sha.to_string();
    let conn = web::block(move || pool.get())
        .await
        .unwrap()
        .map_err(error::ErrorInternalServerError)
        .unwrap();

    web::block(move || {
        conn.prepare("UPDATE git_commit SET build_status = ?1 WHERE sha = ?2 AND repo_id = ?3")
            .unwrap()
            .execute(params![
                to_variant_name(&build_status).unwrap(),
                sha,
                repo_id
            ])
            .unwrap();
    })
    .await
    .unwrap();
    // .map_err(error::ErrorInternalServerError)

    Ok(())
}

//

// pub async fn get_events(
//     pool: &Pool,
//     class_id: i64,
//     user_id: i64,
// ) -> Result<Vec<EventResult>, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn.prepare(
//             "SELECT id, desc, variant, timestamp, event_class_id FROM event WHERE event_class_id = ?1 AND user_id = ?2 ORDER BY timestamp DESC",
//         )?
//         .query_map([class_id, user_id], |row| {
//             Ok(EventResult {
//                 id: row.get(0)?,
//                 desc: row.get(1)?,
//                 variant: row.get::<usize, String>(2)?.try_into().unwrap(),
//                 timestamp: row.get(3)?,
//                 class_id: row.get(4)?,
//             })
//         })
//         .and_then(Iterator::collect)
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }

// pub async fn get_class(
//     pool: &Pool,
//     class_id: i64,
//     user_id: i64,
// ) -> Result<Option<ClassResult>, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn.prepare("SELECT id, name FROM event_class WHERE id = ?1 AND user_id = ?2")?
//             .query_row([class_id, user_id], |row| {
//                 Ok(ClassResult {
//                     id: row.get(0)?,
//                     name: row.get(1)?,
//                 })
//             })
//             .optional()
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }

// pub async fn get_latest_event(
//     pool: &Pool,
//     class_id: i64,
//     user_id: i64,
// ) -> Result<Option<EventResult>, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn.prepare(
//             "SELECT id, desc, timestamp, event_class_id FROM event WHERE event_class_id = ?1 AND user_id = ?2 AND variant = 'positive' ORDER BY timestamp DESC LIMIT 1",
//         )?
//         .query_row([class_id, user_id], |row| {
//             Ok(EventResult {
//                 id: row.get(0)?,
//                 desc: row.get(1)?,
//                 variant: EventVariant::Positive,
//                 timestamp: row.get(2)?,
//                 class_id: row.get(3)?,
//             })
//         })
//         .optional()
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }

// pub async fn get_classes(pool: &Pool, user_id: i64) -> Result<Vec<ClassResult>, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn.prepare("SELECT id, name FROM event_class WHERE deleted_at IS NULL AND user_id = ?1")?
//             .query_map([user_id], |row| {
//                 Ok(ClassResult {
//                     id: row.get(0)?,
//                     name: row.get(1)?,
//                 })
//             })
//             .and_then(Iterator::collect)
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }

// #[derive(Debug, Serialize, Deserialize)]
// pub struct CreateClass {
//     name: String,
// }

// pub async fn insert_class(
//     pool: &Pool,
//     create_class: CreateClass,
//     user_id: i64,
// ) -> Result<ClassResult, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;
//     let name1 = create_class.name.clone();

//     let id = web::block(move || {
//         conn.prepare("INSERT INTO event_class (name, user_id) VALUES (?1, ?2)")?
//             .insert(params![name1, user_id])
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)?;

//     Ok(ClassResult {
//         id,
//         name: create_class.name,
//     })
// }

// pub async fn update_class(
//     pool: &Pool,
//     id: i64,
//     create_class: CreateClass,
//     user_id: i64,
// ) -> Result<ClassResult, Error> {
//     let pool = pool.clone();
//     let pool2 = pool.clone();
//     let conn1 = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;
//     let conn2 = web::block(move || pool2.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn1
//             .prepare("UPDATE event_class SET name = ?1 WHERE id = ?2 AND user_id = ?3")?
//             .execute(params![create_class.name, id, user_id])
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn2
//             .prepare("SELECT id, name FROM event_class WHERE id = ?1 AND user_id = ?2")?
//             .query_row([id, user_id], |row| {
//                 Ok(ClassResult {
//                     id: row.get(0)?,
//                     name: row.get(1)?,
//                 })
//             })
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }

// pub async fn delete_class(pool: &Pool, id: i64, user_id: i64) -> Result<(), Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || -> Result<(), rusqlite::Error> {
//         conn.prepare("UPDATE event_class SET deleted_at = ?1 WHERE id = ?2 AND user_id = ?3")?
//             .execute(params![
//                 SystemTime::now()
//                     .duration_since(UNIX_EPOCH)
//                     .expect("Time went backwards")
//                     .as_secs(),
//                 id,
//                 user_id
//             ])?;

//         Ok(())
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)?;

//     Ok(())
// }

// pub async fn db_delete_event(
//     pool: &Pool,
//     class_id: i64,
//     id: i64,
//     user_id: i64,
// ) -> Result<(), Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || -> Result<(), rusqlite::Error> {
//         conn.prepare("DELETE FROM Event WHERE id = ?1 AND user_id = ?2 AND event_class_id = ?3")?
//             .execute(params![id, user_id, class_id])?;

//         Ok(())
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)?;

//     Ok(())
// }

// #[derive(Debug, Serialize, Deserialize)]
// pub struct CreateEvent {
//     desc: Option<String>,
//     variant: EventVariant,
//     timestamp: i64,
// }

// pub async fn insert_event(
//     pool: &Pool,
//     class_id: i64,
//     create_event: CreateEvent,
//     user_id: i64,
// ) -> Result<EventResult, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;
//     let desc1 = create_event.desc.clone();

//     let id =
//         web::block(move || {
//             conn.prepare(
//             "INSERT INTO event (desc, timestamp, variant, event_class_id, user_id) VALUES (?1, ?2, ?3, ?4, ?5)",
//             )?
//         .insert(params![create_event.desc, create_event.timestamp, create_event.variant.as_string(), class_id, user_id])
//         })
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     Ok(EventResult {
//         id,
//         desc: desc1,
//         variant: create_event.variant,
//         timestamp: create_event.timestamp,
//         class_id,
//     })
// }

// pub async fn fetch_auth_entry(pool: &Pool, username: String) -> Result<Option<AuthRow>, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn.prepare("SELECT id, username, name, hash FROM user WHERE username = ?1")?
//             .query_row([username], |row| {
//                 Ok(AuthRow {
//                     id: row.get(0)?,
//                     username: row.get(1)?,
//                     name: row.get(2)?,
//                     hash: row.get(3)?,
//                 })
//             })
//             .optional()
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }

// pub async fn fetch_profile(pool: &Pool, user_id: i64) -> Result<Option<UserFacingAuthRow>, Error> {
//     let pool = pool.clone();
//     let conn = web::block(move || pool.get())
//         .await?
//         .map_err(error::ErrorInternalServerError)?;

//     web::block(move || {
//         conn.prepare("SELECT id, username, name FROM user WHERE id = ?1")?
//             .query_row([user_id], |row| {
//                 Ok(UserFacingAuthRow {
//                     id: row.get(0)?,
//                     username: row.get(1)?,
//                     name: row.get(2)?,
//                 })
//             })
//             .optional()
//     })
//     .await?
//     .map_err(error::ErrorInternalServerError)
// }
