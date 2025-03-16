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
}

#[derive(Clone, SimpleObject)]
pub struct TrackedBuildAndRepo {
    id: i64,
    sha: String,
    message: String,
    timestamp: i64,
    build_status: Option<String>,
    build_url: Option<String>,
    repo_name: String,
    repo_owner_name: String,
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

        Ok(commits
            .unwrap()
            .into_iter()
            .map(|x| TrackedBuildAndRepo {
                id: x.commit.id,
                sha: x.commit.sha,
                message: x.commit.message,
                timestamp: x.commit.timestamp,
                build_status: Some(to_variant_name(&x.commit.build_status).unwrap().to_string()),
                build_url: x.commit.build_url,
                repo_name: x.repo.name,
                repo_owner_name: x.repo.owner_name,
            })
            .collect())
    }
}

pub async fn index_graphiql() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(GraphiQLSource::build().endpoint("/api/graphql").finish())
}
