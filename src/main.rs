pub mod prelude {
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

    pub use crate::error::{AppError, AppResult};
    pub use actix_web::Error;
    pub use actix_web::{guard, Result};
    pub use async_graphql::{http::GraphiQLSource, EmptyMutation, EmptySubscription, Schema};
    pub use async_graphql_actix_web::GraphQL;
    pub use maud::{html, Markup, DOCTYPE};
    pub use r2d2::PooledConnection;
    pub use rusqlite::Connection;
    pub use rusqlite::{params, OptionalExtension};
    pub use rusqlite_migration::{Migrations, M};
    pub use std::time::{SystemTime, UNIX_EPOCH};
}

mod build_status;
mod crab_ext;
mod db;
mod error;
mod kubernetes;
mod web;
mod webhooks;
use crate::crab_ext::{initialize_octocrabs, Octocrabs};
use crate::db::migrations::migrate;
use crate::kubernetes::controller::start_controller;
use crate::prelude::*;
use crate::web::{branch_grid_fragment, build_grid_fragment, deploy_configs, deploy_preview};
use crate::webhooks::config_sync::ConfigSyncHandler;
use crate::webhooks::database::DatabaseHandler;
use crate::webhooks::manager::WebhookManager;
use cicd::serve_static_file;
use web::{all_recent_builds, deploy_config, index, teams_index, toggle_team};

async fn start_http(
    registry: prometheus::Registry,
    pool: Pool<SqliteConnectionManager>,
    octocrabs: Octocrabs,
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
        let mut app = App::new();

        // Add Kubernetes client data if available
        if let Some(client) = &kube_client {
            app = app
                .app_data(Data::new(client.clone()))
                .service(deploy_config)
        }

        app.wrap(RequestTracing::new())
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
            .app_data(Data::new(octocrabs.clone()))
            .app_data(Data::new(pool.clone()))
            .wrap(middleware::Logger::default())
            .service(deploy_configs)
            .service(index)
            .service(branch_grid_fragment)
            .service(build_grid_fragment)
            .service(all_recent_builds)
            .service(teams_index)
            .service(toggle_team)
            .service(deploy_preview)
            .service(serve_static_file!("htmx.min.js"))
            .service(serve_static_file!("idiomorph.min.js"))
            .service(serve_static_file!("idiomorph-ext.min.js"))
            .service(serve_static_file!("styles.css"))
            .service(serve_static_file!("deploy.css"))
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

async fn start_kubernetes_controller(
    pool: Pool<SqliteConnectionManager>,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Starting Kubernetes controller");

    // Initialize Kubernetes client
    let client = kube::Client::try_default().await?;

    // Start the controller
    start_controller(client, pool).await?;

    Ok(())
}

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
        .filter_module("cicd::kubernetes::deploy_handlers", log::LevelFilter::Debug)
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

    // Initialize Kubernetes client
    let client = kube::Client::try_default()
        .await
        .expect("Failed to initialize Kubernetes client");

    let mut webhook_manager = WebhookManager::new(
        std::env::var("WEBSOCKET_URL").expect("WEBSOCKET_URL must be set"),
        std::env::var("CLIENT_SECRET").expect("CLIENT_SECRET must be set"),
    );
    webhook_manager.add_handler(DatabaseHandler::new(pool.clone(), octocrabs.clone()));
    webhook_manager.add_handler(ConfigSyncHandler::new(
        pool.clone(),
        client.clone(),
        octocrabs.clone(),
    ));

    tokio::select! {
        _ = Box::pin(start_http(
            registry,
            pool.clone(),
            octocrabs.clone(),
        )) => {},
        _ = Box::pin(webhook_manager.start()) => {},
        _ = Box::pin(start_kubernetes_controller(
            pool.clone()
        )) => {}
    };

    Ok(())
}
