use std::sync::Arc;
use std::sync::RwLock;

use crate::error::format_anyhow_chain;
use crate::prelude::*;
use crate::webhooks::models::CheckRunEvent;
use crate::webhooks::models::CheckSuiteEvent;
use crate::webhooks::models::DeleteEvent;
use crate::webhooks::models::PushEvent;
use crate::webhooks::models::WebhookEvent;
use crate::webhooks::WebhookHandler;
use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
};
pub struct WebhookManager {
    handlers: Vec<Arc<dyn WebhookHandler + Send + Sync>>,
    websocket_url: String,
    client_secret: String,
}

impl WebhookManager {
    pub fn new(websocket_url: String, client_secret: String) -> Self {
        Self {
            handlers: Vec::new(),
            websocket_url,
            client_secret,
        }
    }

    pub fn add_handler(&mut self, handler: impl WebhookHandler + Send + Sync + 'static) {
        self.handlers.push(Arc::new(handler));
    }

    pub async fn start(&self) -> Result<(), anyhow::Error> {
        loop {
            log::info!(
                "Attempting to connect to webhook WebSocket at {}",
                self.websocket_url
            );

            let mut request = match self.websocket_url.clone().into_client_request() {
                Ok(request) => request,
                Err(e) => {
                    log::error!("Failed to create WebSocket request: {}", e);
                    // Wait before retrying
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    continue;
                }
            };

            request.headers_mut().insert(
                "Authorization",
                match format!("Bearer {}", self.client_secret).parse::<HeaderValue>() {
                    Ok(header) => header,
                    Err(e) => {
                        log::error!("Failed to create Authorization header: {}", e);
                        // Wait before retrying
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        continue;
                    }
                },
            );

            let connect_result = connect_async(request).await;

            match connect_result {
                Ok((ws_stream, _)) => {
                    log::info!("Connection to webhooks websocket established");

                    let (mut write, mut read) = ws_stream.split();

                    let last_pong = Arc::new(RwLock::new(Box::new(std::time::Instant::now())));
                    let last_pong_clone = last_pong.clone();

                    let mut ping_closure = async || loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        log::debug!("Sending ping message");
                        if let Err(e) = write
                            .send(Message::Text(
                                "{\"event_type\":\"conn_ping\",\"payload\":{}}".to_string(),
                            ))
                            .await
                        {
                            log::error!("Failed to send ping message: {}", e);
                            break;
                        }
                    };

                    let mut message_closure = async || loop {
                        let message = match read.next().await {
                            Some(msg) => msg,
                            None => {
                                log::warn!("WebSocket stream ended");
                                break;
                            }
                        };
                        match message {
                            Ok(msg) => {
                                let data = msg.into_data();
                                if let Ok(mut last_pong) = last_pong_clone.write() {
                                    *last_pong.as_mut() = std::time::Instant::now();
                                }
                                match serde_json::from_slice::<WebhookEvent>(&data) {
                                    Ok(event) => {
                                        if event.event_type == "conn_ping" {
                                            log::debug!("Got conn_ping reply");
                                        } else {
                                            match self.process_event(event).await {
                                                Ok(_) => {}
                                                Err(e) => {
                                                    log::error!(
                                                        "Error processing event: {}",
                                                        format_anyhow_chain(&e)
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => log::error!("Error parsing webhook event: {}", e),
                                }
                            }
                            Err(e) => log::error!("Error reading from websocket: {}", e),
                        }
                    };

                    let watchdog_closure = async || loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
                        let last_pong = match last_pong_clone.read() {
                            Ok(pong) => pong,
                            Err(e) => {
                                log::error!("Failed to read last_pong: {}", e);
                                break;
                            }
                        };
                        if last_pong.elapsed() > tokio::time::Duration::from_secs(10) {
                            log::debug!("Watchdog failed");
                            break;
                        } else {
                            log::debug!("Watchdog passed");
                        }
                    };

                    tokio::select! {
                        _ = ping_closure() => {}
                        _ = message_closure() => {}
                        _ = watchdog_closure() => {}
                    }

                    log::error!("WebSocket connection closed, will attempt to reconnect...");
                }
                Err(e) => {
                    log::error!("Failed to connect to WebSocket: {}", e);
                }
            }

            // Wait before retrying
            log::warn!("Reconnecting in 10 seconds...");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }
    }

    async fn process_event(&self, event: WebhookEvent) -> Result<(), anyhow::Error> {
        log::debug!("Received event: {}", event.event_type);

        match event.event_type.as_str() {
            "push" => match serde_json::from_value::<PushEvent>(event.payload.clone()) {
                Ok(payload) => {
                    for handler in &self.handlers {
                        let handler_result = handler.handle_push(payload.clone()).await;
                        if let Err(e) = handler_result {
                            log::error!("Error handling push:\n{}", format_anyhow_chain(&e));
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::Error::msg(format!(
                    "Error parsing push event: {}",
                    e
                ))),
            },
            "check_run" => match serde_json::from_value::<CheckRunEvent>(event.payload) {
                Ok(payload) => {
                    for handler in &self.handlers {
                        let handler_result = handler.handle_check_run(payload.clone()).await;
                        if let Err(e) = handler_result {
                            log::error!("Error handling check run:\n{}", format_anyhow_chain(&e));
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::Error::msg(format!(
                    "Error parsing check run event: {}",
                    e
                ))),
            },
            "check_suite" => match serde_json::from_value::<CheckSuiteEvent>(event.payload) {
                Ok(payload) => {
                    for handler in &self.handlers {
                        let handler_result = handler.handle_check_suite(payload.clone()).await;
                        if let Err(e) = handler_result {
                            log::error!("Error handling check suite:\n{}", format_anyhow_chain(&e));
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::Error::msg(format!(
                    "Error parsing check suite event: {}",
                    e
                ))),
            },
            "delete" => match serde_json::from_value::<DeleteEvent>(event.payload) {
                Ok(payload) => {
                    for handler in &self.handlers {
                        let handler_result = handler.handle_delete(payload.clone()).await;
                        if let Err(e) = handler_result {
                            log::error!("Error handling delete:\n{}", format_anyhow_chain(&e));
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::Error::msg(format!(
                    "Error parsing delete event: {}",
                    e
                ))),
            },
            _ => {
                log::debug!("Received unknown event: {}", event.event_type);
                for handler in &self.handlers {
                    let handler_result = handler.handle_unknown(&event.event_type).await;
                    if let Err(e) = handler_result {
                        log::error!("Error handling unknown event:\n{}", format_anyhow_chain(&e));
                    }
                }
                Ok(())
            }
        }
    }
}
