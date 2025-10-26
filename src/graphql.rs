// use crate::prelude::*;

// use async_graphql::{Context, Object, Result, SimpleObject};
// use serde_variant::to_variant_name;

// // A Git repository
// #[derive(Clone, SimpleObject)]
// pub struct Repository {
//     pub id: i64,
//     pub name: String,
//     pub owner: String, // Repository owner (username/organization)
//     pub default_branch: String,
//     pub is_private: bool,
//     pub language: Option<String>,
// }

// // A Git commit
// #[derive(Clone, SimpleObject)]
// pub struct Commit {
//     pub id: i64,
//     pub sha: String,
//     pub message: String,
//     pub timestamp: i64,
//     pub author: String, // Just the author name for simplicity
//     pub parent_shas: Vec<String>,
// }

// // A Git branch
// #[derive(Clone, SimpleObject)]
// pub struct Branch {
//     pub id: i64,
//     pub name: String,
//     pub head_commit_sha: String,
// }

// // A CI/CD build based on a commit
// #[derive(Clone, SimpleObject)]
// pub struct Build {
//     pub commit: Commit,
//     pub repository: Repository,
//     pub status: String,        // None, Pending, Success, Failure
//     pub url: Option<String>,   // URL to the build details
//     pub branches: Vec<Branch>, // Branches this build is associated with
// }

// pub struct QueryRoot;

// #[Object]
// impl QueryRoot {
//     // Get builds from the last hour
//     async fn recent_builds(&self, ctx: &Context<'_>) -> Result<Vec<Build>> {
//         let pool = ctx
//             .data_unchecked::<Pool<SqliteConnectionManager>>()
//             .clone();

//         let conn = pool
//             .get()
//             .map_err(|e| format!("Failed to get database connection: {}", e))?;

//         let since = Utc::now() - chrono::Duration::hours(1);
//         let commits = get_commits_since(&conn, since.timestamp_millis())
//             .map_err(|e| format!("Failed to get commits: {}", e))?;

//         let mut builds = Vec::new();

//         for commit_with_repo in commits {
//             let parent_shas = get_commit_parents(&commit_with_repo.commit.sha, &conn)
//                 .map_err(|e| format!("Failed to get commit parents: {}", e))?;
//             let branches = get_branches_for_commit(&commit_with_repo.commit.sha, &conn)
//                 .map_err(|e| format!("Failed to get branches for commit: {}", e))?;

//             let status = to_variant_name(&commit_with_repo.commit.build_status)
//                 .map_err(|e| format!("Failed to serialize build status: {}", e))?
//                 .to_string();

//             builds.push(Build {
//                 commit: Commit {
//                     id: commit_with_repo.commit.id,
//                     sha: commit_with_repo.commit.sha,
//                     message: commit_with_repo.commit.message,
//                     timestamp: commit_with_repo.commit.timestamp,
//                     author: "Unknown".to_string(), // No committer field in the DB yet
//                     parent_shas,
//                 },
//                 repository: Repository {
//                     id: commit_with_repo.repo.id,
//                     name: commit_with_repo.repo.name,
//                     owner: commit_with_repo.repo.owner_name,
//                     default_branch: commit_with_repo.repo.default_branch,
//                     is_private: commit_with_repo.repo.private,
//                     language: commit_with_repo.repo.language,
//                 },
//                 status,
//                 url: commit_with_repo.commit.build_url,
//                 branches: branches
//                     .into_iter()
//                     .map(|b| Branch {
//                         id: b.id,
//                         name: b.name,
//                         head_commit_sha: b.head_commit_sha,
//                     })
//                     .collect(),
//             });
//         }

//         Ok(builds)
//     }

//     // Find a specific build by commit SHA
//     async fn build(&self, ctx: &Context<'_>, sha: String) -> Result<Option<Build>> {
//         let pool = ctx
//             .data_unchecked::<Pool<SqliteConnectionManager>>()
//             .clone();

//         let conn = pool
//             .get()
//             .map_err(|e| format!("Failed to get database connection: {}", e))?;

//         let commit_with_repo_branches = get_commit_with_repo_branches(&sha, &conn)
//             .map_err(|e| format!("Failed to get commit with repo branches: {}", e))?;

//         commit_with_repo_branches
//             .map(|x| -> Result<Build> {
//                 let status = to_variant_name(&x.commit.build_status)
//                     .map_err(|e| format!("Failed to serialize build status: {}", e))?
//                     .to_string();

//                 Ok(Build {
//                     commit: Commit {
//                         id: x.commit.id,
//                         sha: x.commit.sha,
//                         message: x.commit.message,
//                         timestamp: x.commit.timestamp,
//                         author: "Unknown".to_string(), // Need to extend DB model to store author
//                         parent_shas: x.parent_shas,
//                     },
//                     repository: Repository {
//                         id: x.repo.id,
//                         name: x.repo.name,
//                         owner: x.repo.owner_name,
//                         default_branch: x.repo.default_branch,
//                         is_private: x.repo.private,
//                         language: x.repo.language,
//                     },
//                     status,
//                     url: x.commit.build_url,
//                     branches: x
//                         .branches
//                         .into_iter()
//                         .map(|b| Branch {
//                             id: b.id,
//                             name: b.name,
//                             head_commit_sha: b.head_commit_sha,
//                         })
//                         .collect(),
//                 })
//             })
//             .transpose()
//     }

//     // Get parent builds of a specific commit
//     async fn parent_builds(
//         &self,
//         ctx: &Context<'_>,
//         sha: String,
//         max_depth: Option<i32>,
//     ) -> Result<Vec<Build>> {
//         let pool = ctx
//             .data_unchecked::<Pool<SqliteConnectionManager>>()
//             .clone();

//         let conn = pool
//             .get()
//             .map_err(|e| format!("Failed to get database connection: {}", e))?;

//         let commit_repo = get_repo_by_commit_sha(&sha, &conn)
//             .map_err(|e| format!("Failed to get repository by commit SHA: {}", e))?;

//         if let Some(repo) = commit_repo {
//             let max_depth = max_depth.unwrap_or(10).clamp(1, 20) as usize;
//             let parent_commits = get_parent_commits(&sha, &conn, max_depth)
//                 .map_err(|e| format!("Failed to get parent commits: {}", e))?;

//             let mut builds = Vec::new();

//             for commit in parent_commits {
//                 let parent_shas = get_commit_parents(&commit.sha, &conn)
//                     .map_err(|e| format!("Failed to get commit parents: {}", e))?;
//                 let branches = get_branches_for_commit(&commit.sha, &conn)
//                     .map_err(|e| format!("Failed to get branches for commit: {}", e))?;

//                 let status = to_variant_name(&commit.build_status)
//                     .map_err(|e| format!("Failed to serialize build status: {}", e))?
//                     .to_string();

//                 builds.push(Build {
//                     commit: Commit {
//                         id: commit.id,
//                         sha: commit.sha,
//                         message: commit.message,
//                         timestamp: commit.timestamp,
//                         author: "Unknown".to_string(), // Need to extend DB model to store author
//                         parent_shas,
//                     },
//                     repository: Repository {
//                         id: repo.id,
//                         name: repo.name.clone(),
//                         owner: repo.owner_name.clone(),
//                         default_branch: repo.default_branch.clone(),
//                         is_private: repo.private,
//                         language: repo.language.clone(),
//                     },
//                     status,
//                     url: commit.build_url,
//                     branches: branches
//                         .into_iter()
//                         .map(|b| Branch {
//                             id: b.id,
//                             name: b.name,
//                             head_commit_sha: b.head_commit_sha,
//                         })
//                         .collect(),
//                 });
//             }

//             Ok(builds)
//         } else {
//             Ok(Vec::new())
//         }
//     }

//     // Get child builds of a specific commit
//     async fn child_builds(&self, ctx: &Context<'_>, sha: String) -> Result<Vec<Build>> {
//         let pool = ctx
//             .data_unchecked::<Pool<SqliteConnectionManager>>()
//             .clone();

//         let conn = pool
//             .get()
//             .map_err(|e| format!("Failed to get database connection: {}", e))?;

//         // Get child commits
//         let child_commits = get_child_commits(&sha, &conn)
//             .map_err(|e| format!("Failed to get child commits: {}", e))?;

//         let mut builds = Vec::new();

//         for db_commit in child_commits {
//             // Get the repo info for this commit
//             let repo = get_repo_by_commit_sha(&db_commit.sha, &conn)
//                 .map_err(|e| format!("Failed to get repository by commit SHA: {}", e))?
//                 .ok_or_else(|| format!("Repository not found for commit {}", db_commit.sha))?;

//             // Get the parent SHAs
//             let parent_shas = get_commit_parents(&db_commit.sha, &conn)
//                 .map_err(|e| format!("Failed to get commit parents: {}", e))?;

//             // Get branches
//             let branches = get_branches_for_commit(&db_commit.sha, &conn)
//                 .map_err(|e| format!("Failed to get branches for commit: {}", e))?
//                 .into_iter()
//                 .map(|b| Branch {
//                     id: b.id,
//                     name: b.name,
//                     head_commit_sha: b.head_commit_sha,
//                 })
//                 .collect();

//             let status = to_variant_name(&db_commit.build_status)
//                 .map_err(|e| format!("Failed to serialize build status: {}", e))?
//                 .to_string();

//             builds.push(Build {
//                 commit: Commit {
//                     id: db_commit.id,
//                     sha: db_commit.sha,
//                     message: db_commit.message,
//                     timestamp: db_commit.timestamp,
//                     // FIXME: Need to extend DB model to store author
//                     author: "Unknown".to_string(),
//                     parent_shas,
//                 },
//                 repository: Repository {
//                     id: repo.id,
//                     name: repo.name,
//                     owner: repo.owner_name,
//                     default_branch: repo.default_branch,
//                     is_private: repo.private,
//                     language: repo.language,
//                 },
//                 status,
//                 url: db_commit.build_url,
//                 branches,
//             });
//         }

//         Ok(builds)
//     }

//     // Get repositories
//     async fn repositories(&self, ctx: &Context<'_>) -> Result<Vec<Repository>> {
//         let pool = ctx
//             .data_unchecked::<Pool<SqliteConnectionManager>>()
//             .clone();

//         let conn = pool
//             .get()
//             .map_err(|e| format!("Failed to get database connection: {}", e))?;

//         let repos = get_all_repos(&conn)
//             .map_err(|e| format!("Failed to get all repositories: {}", e))?
//             .into_iter()
//             .map(|r| Repository {
//                 id: r.id,
//                 name: r.name,
//                 owner: r.owner_name,
//                 default_branch: r.default_branch,
//                 is_private: r.private,
//                 language: r.language,
//             })
//             .collect();

//         Ok(repos)
//     }

//     // Get branches for a repository
//     async fn branches(&self, ctx: &Context<'_>, repo_id: i64) -> Result<Vec<Branch>> {
//         let pool = ctx
//             .data_unchecked::<Pool<SqliteConnectionManager>>()
//             .clone();

//         let conn = pool
//             .get()
//             .map_err(|e| format!("Failed to get database connection: {}", e))?;

//         let branches = get_branches_by_repo_id(repo_id, &conn)
//             .map_err(|e| format!("Failed to get branches by repo ID: {}", e))?
//             .into_iter()
//             .map(|b| Branch {
//                 id: b.id,
//                 name: b.name,
//                 head_commit_sha: b.head_commit_sha,
//             })
//             .collect();

//         Ok(branches)
//     }
// }

// pub async fn index_graphiql() -> impl Responder {
//     HttpResponse::Ok()
//         .content_type("text/html; charset=utf-8")
//         .body(GraphiQLSource::build().endpoint("/api/graphql").finish())
// }
