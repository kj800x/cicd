pub mod prelude {
    pub use crate::db::*;
    pub use crate::graphql::*;
    pub use crate::portainer::*;
    pub use crate::resource::*;
    pub use crate::webhooks::*;

    pub use chrono::prelude::*;

    pub use actix_session::{storage::CookieSessionStore, Session, SessionMiddleware};
    pub use actix_web::{
        cookie::Key,
        delete, error, get, middleware, post, put,
        web::{self, Json},
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
}

mod db;
mod graphql;
mod portainer;
mod resource;
mod webhooks;

use futures_util::future;
use prometheus::Registry;

use crate::prelude::*;

#[derive(Debug, Clone)]
pub struct PortainerConfig {
    base: String,
    api_key: String,
}

async fn start_http(
    registry: Registry,
    pool: Pool<SqliteConnectionManager>,
) -> Result<(), std::io::Error> {
    log::info!("Starting HTTP server at http://localhost:8080/api");

    HttpServer::new(move || {
        let pconf = PortainerConfig {
            base: std::env::var("PORTAINER_BASE").expect("PORTAINER_BASE must be set"),
            api_key: std::env::var("PORTAINER_API_KEY").expect("PORTAINER_API_KEY must be set"),
        };

        let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
            .data(pconf.clone())
            .data(pool.clone())
            .finish();

        App::new()
            .wrap(RequestTracing::new())
            .wrap(RequestMetrics::default())
            .route(
                "/api/metrics",
                web::get().to(PrometheusMetricsHandler::new(registry.clone())),
            )
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), Key::from(&[0; 64]))
                    .cookie_secure(false)
                    .build(),
            )
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(pconf))
            .wrap(middleware::Logger::default())
            // .service(home_page_omnibus)
            // .service(stats_page_omnibus)
            // .service(event_class_listing)
            // .service(event_class_create)
            // .service(event_class_update)
            // .service(event_class_delete)
            .service(portainer_endpoints)
            .service(portainer_endpoint)
            // .service(record_event)
            // .service(delete_event)
            // .service(profile)
            // .service(login)
            // .service(logout)
            // .service(register)
            .route("/api/hey", web::get().to(manual_hello))
            .service(
                web::resource("/api/graphql")
                    .guard(guard::Post())
                    .to(GraphQL::new(schema)),
            )
            .service(
                web::resource("/api/graphql")
                    .guard(guard::Get())
                    .to(index_graphiql),
            )
    })
    .bind(("127.0.0.1", 8080))?
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

    let websocket_url = std::env::var("WEBSOCKET_URL").expect("WEBSOCKET_URL must be set");
    let client_secret = std::env::var("CLIENT_SECRET").expect("CLIENT_SECRET must be set");
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
