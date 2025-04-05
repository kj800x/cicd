use crate::prelude::*;

use async_graphql::{Context, Object, Result, SimpleObject};
use serde_variant::to_variant_name;

// A Git repository
#[derive(Clone, SimpleObject)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub owner: String, // Repository owner (username/organization)
    pub default_branch: String,
    pub is_private: bool,
    pub language: Option<String>,
}

// A Git commit
#[derive(Clone, SimpleObject)]
pub struct Commit {
    pub id: i64,
    pub sha: String,
    pub message: String,
    pub timestamp: i64,
    pub author: String, // Just the author name for simplicity
    pub parent_shas: Vec<String>,
}

// A Git branch
#[derive(Clone, SimpleObject)]
pub struct Branch {
    pub id: i64,
    pub name: String,
    pub head_commit_sha: String,
}

// A CI/CD build based on a commit
#[derive(Clone, SimpleObject)]
pub struct Build {
    pub commit: Commit,
    pub repository: Repository,
    pub status: String,        // None, Pending, Success, Failure
    pub url: Option<String>,   // URL to the build details
    pub branches: Vec<Branch>, // Branches this build is associated with
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    // Get builds from the last hour
    async fn recent_builds(&self, ctx: &Context<'_>) -> Result<Vec<Build>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let commits = get_commits_since(&conn, since.timestamp_millis());

        let mut builds = Vec::new();

        for commit_with_repo in commits.unwrap() {
            let parent_shas = get_commit_parents(&commit_with_repo.commit.sha, &conn).unwrap();
            let branches = get_branches_for_commit(&commit_with_repo.commit.sha, &conn).unwrap();

            builds.push(Build {
                commit: Commit {
                    id: commit_with_repo.commit.id,
                    sha: commit_with_repo.commit.sha,
                    message: commit_with_repo.commit.message,
                    timestamp: commit_with_repo.commit.timestamp,
                    author: "Unknown".to_string(), // No committer field in the DB yet
                    parent_shas,
                },
                repository: Repository {
                    id: commit_with_repo.repo.id,
                    name: commit_with_repo.repo.name,
                    owner: commit_with_repo.repo.owner_name,
                    default_branch: commit_with_repo.repo.default_branch,
                    is_private: commit_with_repo.repo.private,
                    language: commit_with_repo.repo.language,
                },
                status: to_variant_name(&commit_with_repo.commit.build_status)
                    .unwrap()
                    .to_string(),
                url: commit_with_repo.commit.build_url,
                branches: branches
                    .into_iter()
                    .map(|b| Branch {
                        id: b.id,
                        name: b.name,
                        head_commit_sha: b.head_commit_sha,
                    })
                    .collect(),
            });
        }

        Ok(builds)
    }

    // Find a specific build by commit SHA
    async fn build(&self, ctx: &Context<'_>, sha: String) -> Result<Option<Build>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let commit_with_repo_branches = get_commit_with_repo_branches(&sha, &conn).unwrap();

        Ok(commit_with_repo_branches.map(|x| Build {
            commit: Commit {
                id: x.commit.id,
                sha: x.commit.sha,
                message: x.commit.message,
                timestamp: x.commit.timestamp,
                author: "Unknown".to_string(), // Need to extend DB model to store author
                parent_shas: x.parent_shas,
            },
            repository: Repository {
                id: x.repo.id,
                name: x.repo.name,
                owner: x.repo.owner_name,
                default_branch: x.repo.default_branch,
                is_private: x.repo.private,
                language: x.repo.language,
            },
            status: to_variant_name(&x.commit.build_status).unwrap().to_string(),
            url: x.commit.build_url,
            branches: x
                .branches
                .into_iter()
                .map(|b| Branch {
                    id: b.id,
                    name: b.name,
                    head_commit_sha: b.head_commit_sha,
                })
                .collect(),
        }))
    }

    // Get parent builds of a specific commit
    async fn parent_builds(
        &self,
        ctx: &Context<'_>,
        sha: String,
        max_depth: Option<i32>,
    ) -> Result<Vec<Build>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let commit_repo = conn
            .prepare(
                "SELECT r.id, r.name, r.owner_name, r.default_branch, r.private, r.language
                 FROM git_commit c
                 JOIN git_repo r ON c.repo_id = r.id
                 WHERE c.sha = ?1",
            )
            .unwrap()
            .query_row([&sha], |row| {
                Ok(DbRepo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    owner_name: row.get(2)?,
                    default_branch: row.get(3)?,
                    private: row.get(4)?,
                    language: row.get(5)?,
                })
            })
            .optional()
            .unwrap();

        if let Some(repo) = commit_repo {
            let max_depth = max_depth.unwrap_or(10).clamp(1, 20) as usize;
            let parent_commits = get_parent_commits(&sha, &conn, max_depth).unwrap();

            let mut builds = Vec::new();

            for commit in parent_commits {
                let parent_shas = get_commit_parents(&commit.sha, &conn).unwrap();
                let branches = get_branches_for_commit(&commit.sha, &conn).unwrap();

                builds.push(Build {
                    commit: Commit {
                        id: commit.id,
                        sha: commit.sha,
                        message: commit.message,
                        timestamp: commit.timestamp,
                        author: "Unknown".to_string(), // Need to extend DB model to store author
                        parent_shas,
                    },
                    repository: Repository {
                        id: repo.id,
                        name: repo.name.clone(),
                        owner: repo.owner_name.clone(),
                        default_branch: repo.default_branch.clone(),
                        is_private: repo.private,
                        language: repo.language.clone(),
                    },
                    status: to_variant_name(&commit.build_status).unwrap().to_string(),
                    url: commit.build_url,
                    branches: branches
                        .into_iter()
                        .map(|b| Branch {
                            id: b.id,
                            name: b.name,
                            head_commit_sha: b.head_commit_sha,
                        })
                        .collect(),
                });
            }

            Ok(builds)
        } else {
            Ok(Vec::new())
        }
    }

    // Get child builds of a specific commit
    async fn child_builds(&self, ctx: &Context<'_>, sha: String) -> Result<Vec<Build>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        // Find all commits that have this commit as a parent
        let query =
            "SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url, c.repo_id
                     FROM git_commit c
                     JOIN git_commit_parent p ON c.sha = p.commit_sha
                     WHERE p.parent_sha = ?1";

        let results = conn
            .prepare(query)
            .unwrap()
            .query_map([&sha], |row| {
                let build_status_str: Option<String> = row.get(4)?;
                let status = match &build_status_str {
                    Some(s) => s.clone(),
                    None => "None".to_string(),
                };

                Ok((
                    Commit {
                        id: row.get(0)?,
                        sha: row.get(1)?,
                        message: row.get(2)?,
                        timestamp: row.get(3)?,
                        author: "Unknown".to_string(), // Need to extend DB model to store author
                        parent_shas: Vec::new(),       // Will populate later
                    },
                    status,
                    row.get::<_, Option<String>>(5)?, // build_url
                    row.get::<_, i64>(6)?,            // repo_id
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let mut builds = Vec::new();

        for (commit, status, build_url, repo_id) in results {
            // Get the repo info
            let repo = conn
                .prepare(
                    "SELECT id, name, owner_name, default_branch, private, language
                     FROM git_repo
                     WHERE id = ?1",
                )
                .unwrap()
                .query_row([repo_id], |row| {
                    Ok(Repository {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        owner: row.get(2)?,
                        default_branch: row.get(3)?,
                        is_private: row.get(4)?,
                        language: row.get(5)?,
                    })
                })
                .unwrap();

            // Get the parent SHAs
            let parent_shas = get_commit_parents(&commit.sha, &conn).unwrap();

            // Get branches
            let branches = get_branches_for_commit(&commit.sha, &conn)
                .unwrap()
                .into_iter()
                .map(|b| Branch {
                    id: b.id,
                    name: b.name,
                    head_commit_sha: b.head_commit_sha,
                })
                .collect();

            builds.push(Build {
                commit: Commit {
                    id: commit.id,
                    sha: commit.sha,
                    message: commit.message,
                    timestamp: commit.timestamp,
                    author: commit.author,
                    parent_shas,
                },
                repository: repo,
                status,
                url: build_url,
                branches,
            });
        }

        Ok(builds)
    }

    // Get repositories
    async fn repositories(&self, ctx: &Context<'_>) -> Result<Vec<Repository>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let mut stmt = conn
            .prepare("SELECT id, name, owner_name, default_branch, private, language FROM git_repo")
            .unwrap();

        let repos = stmt
            .query_map([], |row| {
                Ok(Repository {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    owner: row.get(2)?,
                    default_branch: row.get(3)?,
                    is_private: row.get(4)?,
                    language: row.get(5)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        Ok(repos)
    }

    // Get branches for a repository
    async fn branches(&self, ctx: &Context<'_>, repo_id: i64) -> Result<Vec<Branch>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let mut stmt = conn
            .prepare("SELECT id, name, head_commit_sha, repo_id FROM git_branch WHERE repo_id = ?")
            .unwrap();

        let branches = stmt
            .query_map([repo_id], |row| {
                Ok(Branch {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    head_commit_sha: row.get(2)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        Ok(branches)
    }
}

pub async fn index_graphiql() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(GraphiQLSource::build().endpoint("/api/graphql").finish())
}
