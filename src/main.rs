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

    pub use crate::kubernetes::{
        controller::start_controller, DeployConfig, DeployConfigStatus, Repository as K8sRepository,
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

    pub use crate::discord::{setup_discord, DiscordNotifier};
}

mod db;
mod discord;
mod graphql;
mod kubernetes;
mod resource;
mod web;
mod webhooks;

use futures_util::future;
use prometheus::Registry;
use web::{all_recent_builds, deploy_config, index, watchdog};

use crate::discord::setup_discord;
use crate::prelude::*;

async fn start_http(
    registry: Registry,
    pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
) -> Result<(), std::io::Error> {
    log::info!("Starting HTTP server at http://localhost:8080/api");

    // Initialize Kubernetes client for the web handlers
    let kube_client = match kube::Client::try_default().await {
        Ok(client) => {
            log::info!("Successfully initialized Kubernetes client for web handlers");
            Some(client)
        }
        Err(e) => {
            log::warn!(
                "Failed to initialize Kubernetes client for web handlers: {}",
                e
            );
            log::warn!("DeployConfig deploy functionality will be unavailable");
            None
        }
    };

    HttpServer::new(move || {
        let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
            .data(pool.clone())
            .finish();

        let mut app = App::new()
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
            .route("/deploy", web_get().to(deploy_configs))
            .route("/", web_get().to(index))
            .route("/all-recent-builds", web_get().to(all_recent_builds))
            .route("/watchdog", web_get().to(watchdog))
            .route("/assets/htmx.min.js", web_get().to(htmx_js))
            .route(
                "/fragments/deploy-preview/{namespace}/{name}",
                web_get().to(deploy_preview),
            );

        // Add Kubernetes client data if available
        if let Some(client) = &kube_client {
            app = app
                .app_data(Data::new(client.clone()))
                .service(deploy_config)
        }

        app = app
            .service(
                resource("/api/graphql")
                    .guard(guard::Post())
                    .to(GraphQL::new(schema)),
            )
            .service(
                resource("/api/graphql")
                    .guard(guard::Get())
                    .to(index_graphiql),
            );

        // Add Discord notifier to app data if available
        if let Some(notifier) = discord_notifier.clone() {
            app = app.app_data(Data::new(notifier));
        }

        app
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

async fn start_kubernetes_controller(
    pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Starting Kubernetes controller");

    // Initialize Kubernetes client
    let client = kube::Client::try_default().await?;

    // Start the controller
    start_controller(client, pool, discord_notifier).await?;

    Ok(())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Configure logger with custom filter to prioritize Discord logs
    env_logger::builder()
        .filter_level(log::LevelFilter::Info) // Set default level to Info for most modules
        .filter_module("serenity", log::LevelFilter::Warn) // Serenity crate spams at info level
        .filter_module("actix_web::middleware::logger", log::LevelFilter::Warn) // Actix web middleware logs every request at info
        .filter_module("kube_runtime::controller", log::LevelFilter::Warn) // Kubernetes controller logs every reconciliation at info level
        // .filter_module("cicd::discord", log::LevelFilter::Info)
        // .filter_module("cicd::kubernetes", log::LevelFilter::Info)
        // .filter_module("cicd::web", log::LevelFilter::Info)
        .parse_default_env()
        .init();

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

    // Setup Discord notifier
    log::info!("Setting up Discord notifier...");
    let discord_notifier = setup_discord().await;

    match &discord_notifier {
        Some(_) => log::info!("Discord notifier initialized"),
        None => log::warn!("Discord notifier NOT initialized - notifications will be disabled"),
    }

    // Determine if we should run the Kubernetes controller
    let run_k8s_controller = std::env::var("ENABLE_K8S_CONTROLLER")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    if run_k8s_controller {
        log::info!("Kubernetes controller enabled - will start controller");
        // Start all three services: HTTP server, websockets, and K8s controller
        let http_server = start_http(registry, pool.clone(), discord_notifier.clone());
        let websocket_server = start_websockets(
            websocket_url,
            client_secret,
            pool.clone(),
            discord_notifier.clone(),
        );
        let k8s_controller = start_kubernetes_controller(pool.clone(), discord_notifier);

        // Run all services concurrently
        tokio::select! {
            result = http_server => {
                if let Err(e) = result {
                    log::error!("HTTP server error: {:?}", e);
                }
            }
            _ = websocket_server => {
                log::error!("WebSocket server stopped");
            }
            result = k8s_controller => {
                if let Err(e) = result {
                    log::error!("Kubernetes controller error: {:?}", e);
                }
            }
        }
    } else {
        log::info!("Kubernetes controller disabled - will not start controller");
        // Just start HTTP and websocket services
        future::select(
            Box::pin(start_http(registry, pool.clone(), discord_notifier.clone())),
            Box::pin(start_websockets(
                websocket_url,
                client_secret,
                pool.clone(),
                discord_notifier,
            )),
        )
        .await;
    }

    Ok(())
}
