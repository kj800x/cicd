use crate::prelude::*;

use async_graphql::{Context, Object, Result, SimpleObject};
use serde_variant::to_variant_name;

#[derive(Clone, SimpleObject)]
pub struct TrackedBuild {
    id: i64,
    sha: String,
    message: String,
    timestamp: i64,
    build_status: Option<String>,
    build_url: Option<String>,
    parent_shas: Vec<String>,
}

#[derive(Clone, SimpleObject)]
pub struct TrackedBuildAndRepo {
    id: i64,
    sha: String,
    message: String,
    timestamp: i64,
    build_status: Option<String>,
    build_url: Option<String>,
    parent_shas: Vec<String>,
    repo_name: String,
    repo_owner_name: String,
}

#[derive(Clone, SimpleObject)]
pub struct TrackedBranch {
    id: i64,
    name: String,
    head_commit_sha: String,
}

#[derive(Clone, SimpleObject)]
pub struct TrackedBuildWithBranches {
    id: i64,
    sha: String,
    message: String,
    timestamp: i64,
    build_status: Option<String>,
    build_url: Option<String>,
    parent_shas: Vec<String>,
    repo_name: String,
    repo_owner_name: String,
    branches: Vec<TrackedBranch>,
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn recent_builds<'a>(&self, ctx: &Context<'a>) -> Result<Vec<TrackedBuildAndRepo>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let commits = get_commits_since(&conn, since.timestamp_millis());

        let mut tracked_builds = Vec::new();

        for commit_with_repo in commits.unwrap() {
            let parent_shas = get_commit_parents(&commit_with_repo.commit.sha, &conn).unwrap();

            tracked_builds.push(TrackedBuildAndRepo {
                id: commit_with_repo.commit.id,
                sha: commit_with_repo.commit.sha,
                message: commit_with_repo.commit.message,
                timestamp: commit_with_repo.commit.timestamp,
                build_status: Some(
                    to_variant_name(&commit_with_repo.commit.build_status)
                        .unwrap()
                        .to_string(),
                ),
                build_url: commit_with_repo.commit.build_url,
                parent_shas,
                repo_name: commit_with_repo.repo.name,
                repo_owner_name: commit_with_repo.repo.owner_name,
            });
        }

        Ok(tracked_builds)
    }

    async fn recent_builds_with_branches<'a>(
        &self,
        ctx: &Context<'a>,
    ) -> Result<Vec<TrackedBuildWithBranches>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let results = conn
            .prepare(
                "SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url,
                        r.id, r.name, r.owner_name, r.default_branch, r.private, r.language
                 FROM git_commit c
                 JOIN git_repo r ON c.repo_id = r.id
                 WHERE c.timestamp > ?1
                 ORDER BY c.timestamp DESC
                 LIMIT 50",
            )
            .unwrap()
            .query_map([since.timestamp_millis()], |row| {
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
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let mut builds_with_branches = Vec::new();
        for (commit, repo) in results {
            let branches = get_branches_for_commit(&commit.sha, &conn).unwrap();
            let parent_shas = get_commit_parents(&commit.sha, &conn).unwrap();

            builds_with_branches.push(TrackedBuildWithBranches {
                id: commit.id,
                sha: commit.sha,
                message: commit.message,
                timestamp: commit.timestamp,
                build_status: Some(to_variant_name(&commit.build_status).unwrap().to_string()),
                build_url: commit.build_url,
                parent_shas,
                repo_name: repo.name,
                repo_owner_name: repo.owner_name,
                branches: branches
                    .into_iter()
                    .map(|b| TrackedBranch {
                        id: b.id,
                        name: b.name,
                        head_commit_sha: b.head_commit_sha,
                    })
                    .collect(),
            });
        }

        Ok(builds_with_branches)
    }

    async fn commit<'a>(
        &self,
        ctx: &Context<'a>,
        sha: String,
    ) -> Result<Option<TrackedBuildWithBranches>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        let commit_with_repo_branches = get_commit_with_repo_branches(&sha, &conn).unwrap();

        Ok(commit_with_repo_branches.map(|x| TrackedBuildWithBranches {
            id: x.commit.id,
            sha: x.commit.sha,
            message: x.commit.message,
            timestamp: x.commit.timestamp,
            build_status: Some(to_variant_name(&x.commit.build_status).unwrap().to_string()),
            build_url: x.commit.build_url,
            parent_shas: x.parent_shas,
            repo_name: x.repo.name,
            repo_owner_name: x.repo.owner_name,
            branches: x
                .branches
                .into_iter()
                .map(|b| TrackedBranch {
                    id: b.id,
                    name: b.name,
                    head_commit_sha: b.head_commit_sha,
                })
                .collect(),
        }))
    }

    async fn parent_commits<'a>(
        &self,
        ctx: &Context<'a>,
        sha: String,
        max_depth: Option<i32>,
    ) -> Result<Vec<TrackedBuildAndRepo>> {
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
            .unwrap();

        if let Some(repo) = commit_repo {
            let max_depth = max_depth.unwrap_or(10).max(1).min(20) as usize;
            let parent_commits = get_parent_commits(&sha, &conn, max_depth).unwrap();

            let mut tracked_builds = Vec::new();

            for commit in parent_commits {
                let parent_shas = get_commit_parents(&commit.sha, &conn).unwrap();

                tracked_builds.push(TrackedBuildAndRepo {
                    id: commit.id,
                    sha: commit.sha,
                    message: commit.message,
                    timestamp: commit.timestamp,
                    build_status: Some(to_variant_name(&commit.build_status).unwrap().to_string()),
                    build_url: commit.build_url,
                    parent_shas,
                    repo_name: repo.name.clone(),
                    repo_owner_name: repo.owner_name.clone(),
                });
            }

            Ok(tracked_builds)
        } else {
            Ok(Vec::new())
        }
    }

    // New query to get a commit's children
    async fn commit_children<'a>(
        &self,
        ctx: &Context<'a>,
        sha: String,
    ) -> Result<Vec<TrackedBuildAndRepo>> {
        let pool = ctx
            .data_unchecked::<Pool<SqliteConnectionManager>>()
            .clone();

        let conn = pool.get().unwrap();

        // Find all commits that have this commit as a parent
        let results = conn
            .prepare(
                "SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url, c.repo_id
                 FROM git_commit c
                 JOIN git_commit_parent p ON c.sha = p.commit_sha
                 WHERE p.parent_sha = ?1",
            )
            .unwrap()
            .query_map([&sha], |row| {
                Ok((
                    Commit {
                        id: row.get(0)?,
                        sha: row.get(1)?,
                        message: row.get(2)?,
                        timestamp: row.get(3)?,
                        build_status: row.get::<_, Option<String>>(4)?.into(),
                        build_url: row.get(5)?,
                    },
                    row.get::<_, i64>(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let mut children = Vec::new();

        for (commit, repo_id) in results {
            // Get the repo info
            let repo = conn
                .prepare(
                    "SELECT id, name, owner_name, default_branch, private, language
                     FROM git_repo
                     WHERE id = ?1",
                )
                .unwrap()
                .query_row([repo_id], |row| {
                    Ok(Repo {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        owner_name: row.get(2)?,
                        default_branch: row.get(3)?,
                        private: row.get(4)?,
                        language: row.get(5)?,
                    })
                })
                .unwrap();

            // Get the parent SHAs
            let parent_shas = get_commit_parents(&commit.sha, &conn).unwrap();

            children.push(TrackedBuildAndRepo {
                id: commit.id,
                sha: commit.sha,
                message: commit.message,
                timestamp: commit.timestamp,
                build_status: Some(to_variant_name(&commit.build_status).unwrap().to_string()),
                build_url: commit.build_url,
                parent_shas,
                repo_name: repo.name,
                repo_owner_name: repo.owner_name,
            });
        }

        Ok(children)
    }
}

pub async fn index_graphiql() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(GraphiQLSource::build().endpoint("/api/graphql").finish())
}
