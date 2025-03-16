use crate::prelude::*;
use serenity::builder::{CreateMessage, EditMessage};
use serenity::http::Http;
use serenity::model::id::{ChannelId, MessageId};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::Mutex;

// Store Discord message IDs for in-progress builds to update later
#[derive(Clone)]
pub struct DiscordNotifier {
    discord_token: String,
    channel_id: ChannelId,
    http: Arc<Http>,
    // Maps SHA to message ID for updating when builds complete
    message_tracker: Arc<Mutex<HashMap<String, MessageId>>>,
}

impl DiscordNotifier {
    pub fn new() -> Option<Self> {
        // Read environment variables for Discord configuration
        let token = match env::var("DISCORD_BOT_TOKEN") {
            Ok(token) => token,
            Err(_) => {
                log::warn!("DISCORD_BOT_TOKEN not set, Discord notifications are disabled");
                return None;
            }
        };

        let channel_id_str = match env::var("DISCORD_CHANNEL_ID") {
            Ok(id) => id,
            Err(_) => {
                log::warn!("DISCORD_CHANNEL_ID not set, Discord notifications are disabled");
                return None;
            }
        };

        let channel_id = match channel_id_str.parse::<u64>() {
            Ok(id) => ChannelId::new(id),
            Err(_) => {
                log::warn!(
                    "DISCORD_CHANNEL_ID is not a valid ID, Discord notifications are disabled"
                );
                return None;
            }
        };

        let http = Http::new(&token);

        Some(Self {
            discord_token: token,
            channel_id,
            http: Arc::new(http),
            message_tracker: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    // Send a notification when a build starts and store the message ID
    pub async fn notify_build_started(
        &self,
        repo_owner: &str,
        repo_name: &str,
        commit_sha: &str,
        commit_message: &str,
        build_url: Option<&str>,
    ) -> Result<(), String> {
        let content = format!(
            "🔄 **Build Started**\n\
            **Repository:** {}/{}\n\
            **Commit:** {} - {}\n\
            {}",
            repo_owner,
            repo_name,
            &commit_sha[0..7],
            commit_message.lines().next().unwrap_or(commit_message),
            build_url.map_or("".to_string(), |url| format!("**Build URL:** {}", url))
        );

        let message = CreateMessage::new().content(content);

        match self.channel_id.send_message(&self.http, message).await {
            Ok(message) => {
                // Store message ID for later updates
                let mut tracker = self.message_tracker.lock().await;
                tracker.insert(commit_sha.to_string(), message.id);
                Ok(())
            }
            Err(e) => Err(format!("Failed to send Discord message: {}", e)),
        }
    }

    // Update an existing notification when a build completes
    pub async fn notify_build_completed(
        &self,
        repo_owner: &str,
        repo_name: &str,
        commit_sha: &str,
        commit_message: &str,
        build_status: &BuildStatus,
        build_url: Option<&str>,
    ) -> Result<(), String> {
        let status_emoji = match build_status {
            BuildStatus::Success => "✅",
            BuildStatus::Failure => "❌",
            _ => "⚠️",
        };

        let status_text = match build_status {
            BuildStatus::Success => "**Build Succeeded**",
            BuildStatus::Failure => "**Build Failed**",
            BuildStatus::Pending => "**Build In Progress**",
            BuildStatus::None => "**Build Status Unknown**",
        };

        let content = format!(
            "{} {}\n\
            **Repository:** {}/{}\n\
            **Commit:** {} - {}\n\
            {}",
            status_emoji,
            status_text,
            repo_owner,
            repo_name,
            &commit_sha[0..7],
            commit_message.lines().next().unwrap_or(commit_message),
            build_url.map_or("".to_string(), |url| format!("**Build URL:** {}", url))
        );

        // Find message ID from tracker
        let message_id = {
            let tracker = self.message_tracker.lock().await;
            tracker.get(commit_sha).cloned()
        };

        match message_id {
            Some(message_id) => {
                // Update existing message
                let edit_message = EditMessage::new().content(content);

                match self
                    .channel_id
                    .edit_message(&self.http, message_id, edit_message)
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(e) => Err(format!("Failed to update Discord message: {}", e)),
                }
            }
            None => {
                // No previous message found, send a new one
                let message = CreateMessage::new().content(content);

                match self.channel_id.send_message(&self.http, message).await {
                    Ok(_) => Ok(()),
                    Err(e) => Err(format!("Failed to send Discord message: {}", e)),
                }
            }
        }
    }
}

// Add the Discord notifier to the application data
pub async fn setup_discord() -> Option<DiscordNotifier> {
    match DiscordNotifier::new() {
        Some(notifier) => {
            log::info!("Discord notifier initialized successfully");
            Some(notifier)
        }
        None => {
            log::warn!("Discord notifier could not be initialized");
            None
        }
    }
}
