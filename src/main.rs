pub mod prelude {
    pub use crate::db::{
        add_commit_parent, add_commit_to_branch, get_branch_by_name, get_branches_for_commit,
        get_commit, get_commit_parents, get_commit_with_branches, get_commit_with_repo_branches,
        get_commits_since, get_parent_commits, get_repo, migrate, set_commit_status, upsert_branch,
        upsert_commit, upsert_repo, Branch as DbBranch, BuildStatus, Commit as DbCommit,
        CommitParent, CommitWithBranches, CommitWithRepo, CommitWithRepoBranches, Repo as DbRepo,
    };

    pub use crate::graphql::{
        index_graphiql, Branch as GraphQlBranch, Build, Commit as GraphQlCommit, QueryRoot,
        Repository as GraphQlRepository,
    };

    pub use crate::resource::*;

    pub use crate::web::*;

    pub use crate::webhooks::{
        start_websockets, GhCommit, RepoOwner, Repository as WebhookRepository,
    };

    pub use chrono::prelude::*;

    pub use actix_session::{storage::CookieSessionStore, Session, SessionMiddleware};
    pub use actix_web::{
        cookie::Key,
        delete, error, get, middleware, post, put,
        web::{self, get as web_get, resource, Data, Json},
        App, HttpResponse, HttpServer, Responder,
    };
    pub use actix_web_opentelemetry::{PrometheusMetricsHandler, RequestMetrics, RequestTracing};
    pub use futures_util::future::join_all;
    pub use opentelemetry::global;
    pub use opentelemetry_sdk::metrics::MeterProvider;
    pub use r2d2::Pool;
    pub use r2d2_sqlite::SqliteConnectionManager;
    pub use serde::{Deserialize, Serialize};

    pub use actix_web::Error;
    pub use actix_web::{guard, Result};
    pub use async_graphql::{http::GraphiQLSource, EmptyMutation, EmptySubscription, Schema};
    pub use async_graphql_actix_web::GraphQL;
    pub use r2d2::PooledConnection;
    pub use rusqlite::Connection;
    pub use rusqlite::{params, OptionalExtension};
    pub use rusqlite_migration::{Migrations, M};
    pub use std::time::{SystemTime, UNIX_EPOCH};

    // Maud imports
    pub use maud::{html, Markup, DOCTYPE};
}

mod db;
mod graphql;
mod resource;
mod web;
mod webhooks;

use futures_util::future;
use prometheus::Registry;

use crate::prelude::*;

async fn start_http(
    registry: Registry,
    pool: Pool<SqliteConnectionManager>,
) -> Result<(), std::io::Error> {
    log::info!("Starting HTTP server at http://localhost:8080/api");

    HttpServer::new(move || {
        let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
            .data(pool.clone())
            .finish();

        App::new()
            .wrap(RequestTracing::new())
            .wrap(RequestMetrics::default())
            .route(
                "/api/metrics",
                web_get().to(PrometheusMetricsHandler::new(registry.clone())),
            )
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), Key::from(&[0; 64]))
                    .cookie_secure(false)
                    .build(),
            )
            .app_data(Data::new(pool.clone()))
            .wrap(middleware::Logger::default())
            .route("/api/hey", web_get().to(manual_hello))
            .route("/", web_get().to(index))
            .route("/all-recent-builds", web_get().to(all_recent_builds))
            .service(
                resource("/api/graphql")
                    .guard(guard::Post())
                    .to(GraphQL::new(schema)),
            )
            .service(
                resource("/api/graphql")
                    .guard(guard::Get())
                    .to(index_graphiql),
            )
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .unwrap();
    let provider = MeterProvider::builder().with_reader(exporter).build();
    global::set_meter_provider(provider);

    // Get environment variables with defaults for development
    let websocket_url = std::env::var("WEBSOCKET_URL").unwrap_or_else(|_| {
        log::warn!("WEBSOCKET_URL not set, using default for development");
        "wss://example.com/ws".to_string()
    });

    let client_secret = std::env::var("CLIENT_SECRET").unwrap_or_else(|_| {
        log::warn!("CLIENT_SECRET not set, using default for development");
        "development_secret".to_string()
    });

    // connect to SQLite DB
    let manager = SqliteConnectionManager::file(
        std::env::var("DATABASE_PATH").unwrap_or("db.db".to_string()),
    );
    let pool = Pool::new(manager).unwrap();
    migrate(pool.get().unwrap()).unwrap();

    future::select(
        Box::pin(start_http(registry, pool.clone())),
        Box::pin(start_websockets(websocket_url, client_secret, pool.clone())),
    )
    .await;

    Ok(())
}
