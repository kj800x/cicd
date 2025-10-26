pub mod prelude {
    // pub use crate::db::{
    //     add_commit_parent, add_commit_to_branch, get_all_repos, get_branch_by_name,
    //     get_branches_by_repo_id, get_branches_for_commit, get_branches_with_commits,
    //     get_child_commits, get_commit, get_commit_by_sha, get_commit_parents,
    //     get_commit_with_branches, get_commit_with_repo_branches, get_commits_since,
    //     get_latest_successful_build, get_parent_commits, get_repo, get_repo_by_commit_sha,
    //     get_repo_by_id, migrate, set_commit_status, upsert_branch, upsert_commit, upsert_repo,
    //     Branch as DbBranch, BranchWithCommits, BuildStatus, Commit as DbCommit, CommitParent,
    //     CommitWithBranches, CommitWithParents, CommitWithRepo, CommitWithRepoBranches,
    //     Repo as DbRepo,
    // };

    // pub use crate::graphql::{
    //     index_graphiql, Branch as GraphQlBranch, Build, Commit as GraphQlCommit, QueryRoot,
    //     Repository as GraphQlRepository,
    // };

    // pub use crate::resource::*;

    // pub use crate::webhooks::{
    //     start_websockets, GhCommit, RepoOwner, Repository as WebhookRepository,
    // };

    // pub use crate::kubernetes::{
    //     controller::start_controller, DeployConfig, DeployConfigStatus, Repository as K8sRepository,
    // };

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

    // pub use crate::discord::{setup_discord, DiscordNotifier};

    // Error handling
    pub use crate::error::{AppError, AppResult};
}

mod crab_ext;
mod db;
// mod discord;
mod error;
// mod graphql;
// mod kubernetes;
// mod resource;
// mod web;
mod build_status;
mod webhooks;

// use prometheus::Registry;
// use web::{all_recent_builds, deploy_config, index, watchdog};

use crate::crab_ext::{initialize_octocrabs, Octocrabs};
use crate::db::migrations::migrate;
// use crate::discord::setup_discord;
use crate::prelude::*;
// use crate::web::{
//     assets, branch_grid_fragment, build_grid_fragment, deploy_configs, deploy_preview,
// };
use crate::webhooks::database::DatabaseHandler;
use crate::webhooks::log::LogHandler;
use crate::webhooks::manager::WebhookManager;

// async fn start_http(
//     registry: Registry,
//     pool: Pool<SqliteConnectionManager>,
//     // discord_notifier: Option<DiscordNotifier>,
//     octocrabs: Octocrabs,
// ) -> Result<(), std::io::Error> {
//     log::info!("Starting HTTP server at http://localhost:8080/api");

//     // Initialize Kubernetes client for the web handlers
//     let kube_client = match kube::Client::try_default().await {
//         Ok(client) => {
//             log::info!("Successfully initialized Kubernetes client for web handlers");
//             Some(client)
//         }
//         Err(e) => {
//             log::warn!(
//                 "Failed to initialize Kubernetes client for web handlers: {}",
//                 e
//             );
//             log::warn!("DeployConfig deploy functionality will be unavailable");
//             None
//         }
//     };

//     HttpServer::new(move || {
//         // let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
//         //     .data(pool.clone())
//         //     .finish();

//         // let graphql_api = resource("/api/graphql")
//         //     .guard(guard::Post())
//         //     .to(GraphQL::new(schema));
//         // let graphiql_page = resource("/api/graphql")
//         //     .guard(guard::Get())
//         //     .to(index_graphiql);

//         let mut app = App::new();

//         // Add Kubernetes client data if available
//         if let Some(client) = &kube_client {
//             app = app
//                 .app_data(Data::new(client.clone()))
//                 .service(deploy_config)
//         }

//         // Add Discord notifier to app data if available
//         // if let Some(notifier) = discord_notifier.clone() {
//         //     app = app.app_data(Data::new(notifier));
//         // }

//         app.wrap(RequestTracing::new())
//             .wrap(RequestMetrics::default())
//             .route(
//                 "/api/metrics",
//                 web_get().to(PrometheusMetricsHandler::new(registry.clone())),
//             )
//             .wrap(
//                 SessionMiddleware::builder(CookieSessionStore::default(), Key::from(&[0; 64]))
//                     .cookie_secure(false)
//                     .build(),
//             )
//             .app_data(Data::new(octocrabs.clone()))
//             .app_data(Data::new(pool.clone()))
//             .wrap(middleware::Logger::default())
//             .service(manual_hello)
//             .service(sync_all_deploy_configs)
//             .service(sync_repo_deploy_configs)
//             .service(deploy_configs)
//             .service(index)
//             .service(branch_grid_fragment)
//             .service(build_grid_fragment)
//             .service(all_recent_builds)
//             .service(watchdog)
//             .service(deploy_preview)
//             // .service(graphql_api)
//             // .service(graphiql_page)
//             .service(assets())
//     })
//     .bind(("0.0.0.0", 8080))?
//     .run()
//     .await
// }

// async fn start_kubernetes_controller(
//     pool: Pool<SqliteConnectionManager>,
//     enable_k8s_controller: bool,
//     discord_notifier: Option<DiscordNotifier>,
// ) -> Result<(), Box<dyn std::error::Error>> {
//     if !enable_k8s_controller {
//         // FIXME: Hold this future open?
//     }

//     log::info!("Starting Kubernetes controller");

//     // Initialize Kubernetes client
//     let client = kube::Client::try_default().await?;

//     // Start the controller
//     start_controller(client, pool, discord_notifier).await?;

//     Ok(())
// }

#[actix_web::main]
#[allow(clippy::expect_used)]
async fn main() -> std::io::Result<()> {
    let octocrabs: Octocrabs = initialize_octocrabs();

    // Configure logger with custom filter to prioritize Discord logs
    env_logger::builder()
        .filter_level(log::LevelFilter::Info) // Set default level to Info for most modules
        .filter_module("serenity", log::LevelFilter::Warn) // Serenity crate spams at info level
        .filter_module("actix_web::middleware::logger", log::LevelFilter::Warn) // Actix web middleware logs every request at info
        .filter_module("kube_runtime::controller", log::LevelFilter::Warn) // Kubernetes controller logs every reconciliation at info level
        .filter_module("cicd::discord", log::LevelFilter::Info)
        .filter_module("cicd::kubernetes", log::LevelFilter::Info)
        .filter_module("cicd::web", log::LevelFilter::Info)
        .parse_default_env()
        .init();

    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .expect("Failed to build OpenTelemetry Prometheus exporter");
    let provider = MeterProvider::builder().with_reader(exporter).build();
    global::set_meter_provider(provider);

    // connect to SQLite DB
    let manager = SqliteConnectionManager::file(
        std::env::var("DATABASE_PATH").unwrap_or("db.db".to_string()),
    );
    let pool = Pool::new(manager).expect("Failed to create database pool");
    {
        let conn = pool.get().expect("Failed to get database connection");
        migrate(conn).expect("Failed to run database migrations");
    }

    // Setup Discord notifier
    // log::info!("Setting up Discord notifier...");
    // let discord_notifier = setup_discord().await;

    // match &discord_notifier {
    //     Some(_) => log::info!("Discord notifier initialized"),
    //     None => log::warn!("Discord notifier NOT initialized - notifications will be disabled"),
    // }

    // // Determine if we should run the Kubernetes controller
    // let enable_k8s_controller = std::env::var("ENABLE_K8S_CONTROLLER")
    //     .map(|v| v.to_lowercase() == "true")
    //     .unwrap_or(false);

    let mut webhook_manager = WebhookManager::new(
        std::env::var("WEBSOCKET_URL").expect("WEBSOCKET_URL must be set"),
        std::env::var("CLIENT_SECRET").expect("CLIENT_SECRET must be set"),
    );
    webhook_manager.add_handler(LogHandler::new());
    webhook_manager.add_handler(DatabaseHandler::new(pool.clone(), octocrabs.clone()));
    webhook_manager
        .start()
        .await
        .expect("Webhook manager crashed");

    // tokio::select! {
    //     _ = Box::pin(start_http(
    //         registry,
    //         pool.clone(),
    //         discord_notifier.clone(),
    //         octocrabs.clone(),
    //     )) =>  {},
    //     _ = Box::pin(start_websockets(
    //         pool.clone(),
    //         discord_notifier.clone(),
    //         octocrabs.clone(),
    //     )) => {},
    //     _ = Box::pin(start_kubernetes_controller(
    //         pool.clone(),
    //         enable_k8s_controller,
    //         discord_notifier,
    //     )) => {}
    // };

    Ok(())
}
