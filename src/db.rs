use crate::prelude::*;
use rusqlite::Error;
use serde_variant::to_variant_name;

#[derive(Debug, Serialize, Deserialize)]
pub enum BuildStatus {
    None,
    Pending,
    Success,
    Failure,
}

impl From<Option<String>> for BuildStatus {
    fn from(s: Option<String>) -> Self {
        match s {
            Some(s) => match s.as_str() {
                "None" => BuildStatus::None,
                "Pending" => BuildStatus::Pending,
                "Success" => BuildStatus::Success,
                "Failure" => BuildStatus::Failure,
                _ => BuildStatus::None,
            },
            None => BuildStatus::None,
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
    pub build_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommitParent {
    pub sha: String,
    pub parent_sha: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommitWithRepo {
    pub commit: Commit,
    pub repo: Repo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommitWithBranches {
    pub commit: Commit,
    pub branches: Vec<Branch>,
    pub parent_shas: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommitWithRepoBranches {
    pub commit: Commit,
    pub repo: Repo,
    pub branches: Vec<Branch>,
    pub parent_shas: Vec<String>,
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
                FOREIGN KEY(repo_id) REFERENCES git_repo(id)
            );

            CREATE TABLE git_commit (
                id INTEGER PRIMARY KEY NOT NULL,
                sha TEXT NOT NULL,
                message TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                build_status TEXT,
                build_url TEXT,
                repo_id INTEGER NOT NULL,
                FOREIGN KEY(repo_id) REFERENCES git_repo(id)
            );

            CREATE INDEX idx_git_repo_owner_name ON git_repo(owner_name, name);
            CREATE INDEX idx_git_branch_repo_id ON git_branch(repo_id);
            CREATE INDEX idx_git_branch_repo_name ON git_branch(repo_id, name);
            CREATE INDEX idx_git_commit_sha ON git_commit(sha);
            ",
        ),
        M::up(
            "ALTER TABLE git_commit ADD COLUMN parent_sha TEXT;

            CREATE TABLE git_commit_branch (
                commit_sha TEXT NOT NULL,
                branch_id INTEGER NOT NULL,
                repo_id INTEGER NOT NULL,
                PRIMARY KEY (commit_sha, branch_id),
                FOREIGN KEY(branch_id) REFERENCES git_branch(id),
                FOREIGN KEY(repo_id) REFERENCES git_repo(id)
            );

            CREATE INDEX idx_git_commit_branch_commit ON git_commit_branch(commit_sha);
            CREATE INDEX idx_git_commit_branch_branch ON git_commit_branch(branch_id);
            CREATE INDEX idx_git_commit_branch_repo ON git_commit_branch(repo_id);
            ",
        ),
        M::up(
            "CREATE TABLE git_commit_parent (
                commit_sha TEXT NOT NULL,
                parent_sha TEXT NOT NULL,
                repo_id INTEGER NOT NULL,
                PRIMARY KEY (commit_sha, parent_sha),
                FOREIGN KEY(repo_id) REFERENCES git_repo(id)
            );

            -- Migrate existing parent_sha data to the new table
            INSERT INTO git_commit_parent (commit_sha, parent_sha, repo_id)
            SELECT sha, parent_sha, repo_id FROM git_commit
            WHERE parent_sha IS NOT NULL;

            CREATE INDEX idx_git_commit_parent_commit ON git_commit_parent(commit_sha);
            CREATE INDEX idx_git_commit_parent_parent ON git_commit_parent(parent_sha);
            CREATE INDEX idx_git_commit_parent_repo ON git_commit_parent(repo_id);
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
                "UPDATE git_commit SET message = ?3, timestamp = ?4 WHERE sha = ?1 AND repo_id = ?2",
            )
            .unwrap()
            .execute(params![
                commit.id,
                repo_id,
                commit.message,
                DateTime::parse_from_rfc3339(&commit.timestamp)
                    .unwrap()
                    .timestamp_millis()
            ])
            .unwrap();

            // Add parent commits
            if let Some(parents) = &commit.parent_shas {
                for parent_sha in parents {
                    add_commit_parent(&commit.id, parent_sha, repo_id, conn)?;
                }
            }

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

            let commit_id = conn.last_insert_rowid() as u64;

            // Add parent commits
            if let Some(parents) = &commit.parent_shas {
                for parent_sha in parents {
                    add_commit_parent(&commit.id, parent_sha, repo_id, conn)?;
                }
            }

            Ok(commit_id)
        }
    }
}

pub fn add_commit_parent(
    commit_sha: &str,
    parent_sha: &str,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), Error> {
    // Check if this relationship already exists
    let existing = conn
        .prepare("SELECT 1 FROM git_commit_parent WHERE commit_sha = ?1 AND parent_sha = ?2")
        .unwrap()
        .query_row(params![commit_sha, parent_sha], |_| Ok(()))
        .optional();

    // If it doesn't exist, insert it
    if existing.unwrap().is_none() {
        conn.prepare(
            "INSERT INTO git_commit_parent (commit_sha, parent_sha, repo_id) VALUES (?1, ?2, ?3)",
        )
        .unwrap()
        .execute(params![commit_sha, parent_sha, repo_id])
        .unwrap();
    }

    Ok(())
}

pub fn get_commit_parents(
    commit_sha: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare("SELECT parent_sha FROM git_commit_parent WHERE commit_sha = ?1")
        .unwrap();

    let parents_iter = stmt
        .query_map([commit_sha], |row| Ok(row.get::<_, String>(0)?))
        .unwrap();

    let mut parents = Vec::new();
    for parent in parents_iter {
        parents.push(parent?);
    }

    Ok(parents)
}

pub fn get_commit_with_branches(
    commit_sha: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<CommitWithBranches>, Error> {
    let commit_result = conn
        .prepare(
            "SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url
             FROM git_commit c
             WHERE c.sha = ?1",
        )
        .unwrap()
        .query_row([commit_sha], |row| {
            Ok(Commit {
                id: row.get(0)?,
                sha: row.get(1)?,
                message: row.get(2)?,
                timestamp: row.get(3)?,
                build_status: row.get::<_, Option<String>>(4)?.into(),
                build_url: row.get(5)?,
            })
        })
        .optional()
        .unwrap();

    match commit_result {
        Some(commit) => {
            let branches = get_branches_for_commit(commit_sha, conn)?;
            let parent_shas = get_commit_parents(commit_sha, conn)?;
            Ok(Some(CommitWithBranches {
                commit,
                branches,
                parent_shas,
            }))
        }
        None => Ok(None),
    }
}

pub fn get_commit_with_repo_branches(
    commit_sha: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<CommitWithRepoBranches>, Error> {
    let commit_with_repo_result = conn
        .prepare(
            "SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url,
                    r.id, r.name, r.owner_name, r.default_branch, r.private, r.language
             FROM git_commit c
             JOIN git_repo r ON c.repo_id = r.id
             WHERE c.sha = ?1",
        )
        .unwrap()
        .query_row([commit_sha], |row| {
            Ok((
                Commit {
                    id: row.get(0)?,
                    sha: row.get(1)?,
                    message: row.get(2)?,
                    timestamp: row.get(3)?,
                    build_status: row.get::<_, Option<String>>(4)?.into(),
                    build_url: row.get(5)?,
                },
                Repo {
                    id: row.get(6)?,
                    name: row.get(7)?,
                    owner_name: row.get(8)?,
                    default_branch: row.get(9)?,
                    private: row.get(10)?,
                    language: row.get(11)?,
                },
            ))
        })
        .optional()
        .unwrap();

    match commit_with_repo_result {
        Some((commit, repo)) => {
            let branches = get_branches_for_commit(&commit.sha, conn)?;
            let parent_shas = get_commit_parents(&commit.sha, conn)?;
            Ok(Some(CommitWithRepoBranches {
                commit,
                repo,
                branches,
                parent_shas,
            }))
        }
        None => Ok(None),
    }
}

pub fn get_parent_commits(
    commit_sha: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
    max_depth: usize,
) -> Result<Vec<Commit>, Error> {
    let mut result = Vec::new();
    let mut to_process = vec![commit_sha.to_string()];
    let mut processed = std::collections::HashSet::new();
    let mut depth = 0;

    while !to_process.is_empty() && depth < max_depth {
        let mut new_to_process = Vec::new();

        for sha in &to_process {
            if processed.contains(sha) {
                continue;
            }

            processed.insert(sha.clone());

            // Get parents for this commit
            let parents = get_commit_parents(sha, conn)?;

            for parent_sha in parents {
                let parent_commit = conn
                    .prepare(
                        "SELECT id, sha, message, timestamp, build_status, build_url
                         FROM git_commit
                         WHERE sha = ?1",
                    )
                    .unwrap()
                    .query_row([&parent_sha], |row| {
                        Ok(Commit {
                            id: row.get(0)?,
                            sha: row.get(1)?,
                            message: row.get(2)?,
                            timestamp: row.get(3)?,
                            build_status: row.get::<_, Option<String>>(4)?.into(),
                            build_url: row.get(5)?,
                        })
                    })
                    .optional()
                    .unwrap();

                if let Some(commit) = parent_commit {
                    result.push(commit);
                    new_to_process.push(parent_sha);
                }
            }
        }

        to_process = new_to_process;
        depth += 1;
    }

    Ok(result)
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
        "SELECT id, sha, message, timestamp, build_status, build_url FROM git_commit WHERE sha = ?1 AND repo_id = ?2 LIMIT 1",
    ).unwrap()
    .query_row(params![commit_sha, repo_id], |row| {
        Ok(Commit {
            id: row.get_unwrap(0),
            sha: row.get_unwrap(1),
            message: row.get_unwrap(2),
            timestamp: row.get_unwrap(3),
            build_status: row.get_unwrap::<usize, Option<String>>(4).into(),
            build_url: row.get_unwrap::<usize, Option<String>>(5),
        })
    })
    .optional().unwrap())
}

pub fn get_commits_since(
    conn: &PooledConnection<SqliteConnectionManager>,
    since: i64,
) -> Result<Vec<CommitWithRepo>, Error> {
    let mut stmt = conn.prepare(
            "SELECT id, sha, message, timestamp, build_status, build_url, repo_id FROM git_commit WHERE timestamp > ?1 ORDER BY timestamp DESC",
        ).unwrap();
    let mut rows = stmt.query(params![since]).unwrap();
    let mut commits = Vec::new();

    while let Some(row) = rows.next().unwrap() {
        let repo_id = row.get::<usize, i64>(6).unwrap();

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

        let commit = Commit {
            id: row.get_unwrap(0),
            sha: row.get_unwrap(1),
            message: row.get_unwrap(2),
            timestamp: row.get_unwrap(3),
            build_status: row.get_unwrap::<usize, Option<String>>(4).into(),
            build_url: row.get_unwrap::<usize, Option<String>>(5),
        };

        commits.push(CommitWithRepo { repo, commit });
    }

    Ok(commits)
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

            // Add this commit to the branch in the commit-branch relationship table
            add_commit_to_branch(&sha, id, repo_id, conn).unwrap();

            Ok(id)
        }
        None => {
            conn.prepare(
                "INSERT INTO git_branch (name, repo_id, head_commit_sha) VALUES (?1, ?2, ?3)",
            )
            .unwrap()
            .execute(params![name, repo_id, sha])
            .unwrap();

            let branch_id = conn.last_insert_rowid() as u64;

            // Add this commit to the branch in the commit-branch relationship table
            add_commit_to_branch(&sha, branch_id, repo_id, conn).unwrap();

            Ok(branch_id)
        }
    }
}

pub fn set_commit_status(
    sha: &str,
    build_status: BuildStatus,
    build_url: String,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), Error> {
    let sha = sha.to_string();

    conn.prepare(
        "UPDATE git_commit SET build_status = ?1, build_url = ?2 WHERE sha = ?3 AND repo_id = ?4",
    )
    .unwrap()
    .execute(params![
        to_variant_name(&build_status).unwrap(),
        build_url,
        sha,
        repo_id
    ])
    .unwrap();

    Ok(())
}

pub fn add_commit_to_branch(
    commit_sha: &str,
    branch_id: u64,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), Error> {
    // Check if this relationship already exists
    let existing = conn
        .prepare("SELECT 1 FROM git_commit_branch WHERE commit_sha = ?1 AND branch_id = ?2")
        .unwrap()
        .query_row(params![commit_sha, branch_id], |_| Ok(()))
        .optional();

    // If it doesn't exist, insert it
    if existing.unwrap().is_none() {
        conn.prepare(
            "INSERT INTO git_commit_branch (commit_sha, branch_id, repo_id) VALUES (?1, ?2, ?3)",
        )
        .unwrap()
        .execute(params![commit_sha, branch_id, repo_id])
        .unwrap();
    }

    Ok(())
}

pub fn get_branch_by_name(
    branch_name: &str,
    repo_id: u64,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<Branch>, Error> {
    conn.prepare(
        "SELECT id, name, head_commit_sha, repo_id FROM git_branch WHERE name = ?1 AND repo_id = ?2",
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
    .optional()
}

pub fn get_branches_for_commit(
    commit_sha: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Vec<Branch>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT b.id, b.name, b.head_commit_sha, b.repo_id
             FROM git_branch b
             JOIN git_commit_branch cb ON b.id = cb.branch_id
             WHERE cb.commit_sha = ?1",
        )
        .unwrap();

    let branches_iter = stmt
        .query_map([commit_sha], |row| {
            Ok(Branch {
                id: row.get(0)?,
                name: row.get(1)?,
                head_commit_sha: row.get(2)?,
                repo_id: row.get(3)?,
            })
        })
        .unwrap();

    let mut branches = Vec::new();
    for branch in branches_iter {
        branches.push(branch?);
    }

    Ok(branches)
}
