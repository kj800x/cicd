use crate::prelude::*;
use serenity::all::Colour;
use serenity::builder::{CreateEmbed, CreateMessage, EditMessage};
use serenity::http::Http;
use serenity::model::id::{ChannelId, MessageId};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::Mutex;

// Store Discord message IDs for in-progress builds to update later
#[derive(Clone)]
pub struct DiscordNotifier {
    // We keep the token for documentation/debugging purposes only
    #[allow(dead_code)]
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
            Ok(token) => {
                log::debug!("DISCORD_BOT_TOKEN found");
                token
            }
            Err(e) => {
                log::error!("DISCORD_BOT_TOKEN not set: {}", e);
                return None;
            }
        };

        let channel_id_str = match env::var("DISCORD_CHANNEL_ID") {
            Ok(id) => {
                log::debug!("DISCORD_CHANNEL_ID found: {}", id);
                id
            }
            Err(e) => {
                log::error!("DISCORD_CHANNEL_ID not set: {}", e);
                return None;
            }
        };

        let channel_id = match channel_id_str.parse::<u64>() {
            Ok(id) => ChannelId::new(id),
            Err(e) => {
                log::error!("DISCORD_CHANNEL_ID is not a valid ID: {}", e);
                return None;
            }
        };

        log::debug!("Initializing Discord HTTP client");
        let http = Http::new(&token);

        // Validate the token by making a test API call
        let http = Arc::new(http);
        let http_clone = http.clone();
        tokio::spawn(async move {
            match http_clone.get_current_application_info().await {
                Ok(info) => {
                    log::info!("Discord bot validated: {} (ID: {})", info.name, info.id);
                }
                Err(e) => {
                    log::error!("Discord bot validation failed: {}", e);
                    log::error!("Please check if your DISCORD_BOT_TOKEN is correct and the bot is properly set up");
                }
            }
        });

        Some(Self {
            discord_token: token,
            channel_id,
            http,
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
        log::debug!(
            "Preparing Discord notification for build start: {}/{} commit {}",
            repo_owner,
            repo_name,
            commit_sha
        );

        // Create an embed for the build started notification
        let mut embed: CreateEmbed = CreateEmbed::new()
            .title(format!("🔄 Build Started: {}/{}", repo_owner, repo_name))
            .description(format!(
                "A new build has started for commit `{}`",
                &commit_sha[0..7]
            ))
            .color(Colour::BLUE) // Blue for "in progress"
            .field("Repository", format!("{}/{}", repo_owner, repo_name), true)
            .field("Commit", &commit_sha[0..7], true)
            .field(
                "Message",
                commit_message.lines().next().unwrap_or(commit_message),
                false,
            );

        // Add build URL if available
        if let Some(url) = build_url {
            embed = embed.field("Build URL", format!("[View Logs]({})", url), false);
        }

        // Create the message with the embed
        let message = CreateMessage::new().add_embed(embed);

        log::debug!("Sending Discord message to channel ID: {}", self.channel_id);

        match self.channel_id.send_message(&self.http, message).await {
            Ok(message) => {
                log::info!(
                    "Discord build notification sent for {}/{} ({})",
                    repo_owner,
                    repo_name,
                    &commit_sha[0..7]
                );

                // Store message ID for later updates
                let mut tracker = self.message_tracker.lock().await;
                tracker.insert(commit_sha.to_string(), message.id);
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to send Discord message: {}", e);

                // Check if it's a permission issue
                if e.to_string().contains("Missing Permissions") {
                    log::error!("Bot lacks permissions to post in the channel");
                    log::error!("Required permissions: Send Messages, Embed Links");
                } else if e.to_string().contains("Unknown Channel") {
                    log::error!("Channel not found - check if the channel ID is correct and the bot is in the server");
                } else if e.to_string().contains("Unauthorized") {
                    log::error!("Unauthorized - check if the bot token is correct");
                } else if e.to_string().contains("ratelimited") {
                    log::error!("Hit Discord rate limit - consider sending fewer messages");
                }
                Err(format!("Failed to send Discord message: {}", e))
            }
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
        log::debug!(
            "Preparing Discord notification for build completion: {}/{} commit {} with status {:?}",
            repo_owner,
            repo_name,
            commit_sha,
            build_status
        );

        // Set status-specific values (emoji, title, color)
        let (status_emoji, status_title, status_color) = match build_status {
            BuildStatus::Success => ("✅", "Build Succeeded", Colour::DARK_GREEN),
            BuildStatus::Failure => ("❌", "Build Failed", Colour::RED),
            BuildStatus::Pending => ("🔄", "Build In Progress", Colour::GOLD),
            BuildStatus::None => ("⚠️", "Build Status Unknown", Colour::DARK_GREY),
        };

        // Create builder function to generate a consistent embed
        let create_embed = || -> CreateEmbed {
            let mut embed = CreateEmbed::new()
                .title(format!(
                    "{} {}: {}/{}",
                    status_emoji, status_title, repo_owner, repo_name
                ))
                .description(format!(
                    "{}",
                    match build_status {
                        BuildStatus::Success | BuildStatus::Failure =>
                            format!("Build completed for commit `{}`", &commit_sha[0..7]),
                        BuildStatus::Pending | BuildStatus::None =>
                            format!("Build update for commit `{}`", &commit_sha[0..7]),
                    }
                ))
                .color(status_color)
                .field("Repository", format!("{}/{}", repo_owner, repo_name), true)
                .field("Commit", &commit_sha[0..7], true)
                .field("Status", status_title, true)
                .field(
                    "Message",
                    commit_message.lines().next().unwrap_or(commit_message),
                    false,
                );

            // Add build URL if available
            if let Some(url) = build_url {
                embed = embed.field("Build URL", format!("[View Logs]({})", url), false);
            }

            embed
        };

        // Find message ID from tracker
        let message_id = {
            let tracker = self.message_tracker.lock().await;
            tracker.get(commit_sha).cloned()
        };

        if let Some(message_id) = message_id {
            log::debug!("Updating existing Discord message, ID: {}", message_id);
            // Update existing message
            let edit_message = EditMessage::new().embed(create_embed());

            match self
                .channel_id
                .edit_message(&self.http, message_id, edit_message)
                .await
            {
                Ok(_) => {
                    log::info!(
                        "Discord build status updated: {} for {}/{} ({})",
                        status_title,
                        repo_owner,
                        repo_name,
                        &commit_sha[0..7]
                    );
                    Ok(())
                }
                Err(e) => {
                    log::error!("Failed to update Discord message: {}", e);

                    if e.to_string().contains("Unknown Message") {
                        log::warn!("Message not found - it may have been deleted");
                        // Try sending a new message instead
                        self.send_new_discord_message(create_embed()).await
                    } else {
                        Err(format!("Failed to update Discord message: {}", e))
                    }
                }
            }
        } else {
            log::debug!(
                "No existing message found for commit {}, sending new message",
                commit_sha
            );
            self.send_new_discord_message(create_embed()).await
        }
    }

    // Helper method to send a new message with an embed
    async fn send_new_discord_message(&self, embed: CreateEmbed) -> Result<(), String> {
        log::debug!(
            "Sending new Discord message to channel ID: {}",
            self.channel_id
        );

        let message: CreateMessage = CreateMessage::new().add_embed(embed);

        match self.channel_id.send_message(&self.http, message).await {
            Ok(_) => {
                log::info!("New Discord notification sent");
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to send new Discord message: {}", e);

                if e.to_string().contains("Missing Permissions") {
                    log::error!("Bot lacks permissions to post in the channel");
                } else if e.to_string().contains("Unknown Channel") {
                    log::error!("Channel not found - check if the channel ID is correct");
                }

                Err(format!("Failed to send new Discord message: {}", e))
            }
        }
    }

    // Helper to verify channel exists and bot has permission to post
    pub async fn validate_channel(&self) -> Result<(), String> {
        log::debug!("Validating Discord channel (ID: {})", self.channel_id);

        // Only check if the channel exists without sending any test messages
        match self.channel_id.to_channel(&self.http).await {
            Ok(_) => {
                log::debug!("Discord channel verified successfully");
                Ok(())
            }
            Err(e) => {
                log::error!("Channel validation failed: {}", e);

                // Provide more specific error guidance
                if e.to_string().contains("Missing Access") {
                    log::error!("Bot does not have access to the channel - may need to be invited to the server");
                } else if e.to_string().contains("Unknown Channel") {
                    log::error!(
                        "Channel ID {} is invalid or inaccessible - double-check the ID",
                        self.channel_id
                    );
                }

                Err(format!("Channel validation failed: {}", e))
            }
        }
    }

    /// Send a notification when a Kubernetes deployment is created or updated
    pub async fn notify_k8s_deployment(
        &self,
        owner: &str,
        repo_name: &str,
        branch_name: &str,
        commit_sha: &str,
        deployment_name: &str,
        namespace: &str,
        action: &str, // "created" or "updated"
    ) -> Result<(), String> {
        log::debug!(
            "Preparing Discord notification for K8s deployment: {}/{} -> {}",
            owner,
            repo_name,
            deployment_name
        );

        // Choose color based on action
        let color = if action == "created" {
            Colour::BLITZ_BLUE
        } else {
            Colour::DARK_GREEN
        };

        // Choose emoji based on action
        let emoji = if action == "created" {
            "🚀"
        } else {
            "♻️"
        };

        // Create an embed for the deployment notification
        let embed = CreateEmbed::new()
            .title(format!(
                "{} Deployment {}: {}",
                emoji, action, deployment_name
            ))
            .description(format!(
                "Kubernetes deployment has been {} in namespace `{}`",
                action, namespace
            ))
            .color(color)
            .field("Repository", format!("{}/{}", owner, repo_name), true)
            .field("Branch", branch_name, true)
            .field(
                "Commit",
                if commit_sha.len() >= 7 {
                    &commit_sha[0..7]
                } else {
                    commit_sha
                },
                true,
            );

        // Send the message
        let msg = self
            .channel_id
            .send_message(&self.http, CreateMessage::new().embed(embed))
            .await;

        match msg {
            Ok(_) => {
                log::info!(
                    "Discord notification sent for deployment {} in namespace {}",
                    deployment_name,
                    namespace
                );
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to send Discord notification: {:?}", e);
                Err(format!("Discord error: {:?}", e))
            }
        }
    }
}

// Add the Discord notifier to the application data
pub async fn setup_discord() -> Option<DiscordNotifier> {
    log::info!("Setting up Discord notifier");

    // Disable serenity logging

    // Check if Discord environment variables are present
    let has_token = env::var("DISCORD_BOT_TOKEN").is_ok();
    let has_channel = env::var("DISCORD_CHANNEL_ID").is_ok();

    if !has_token || !has_channel {
        log::error!(
            "Missing required Discord environment variables - notifications will be disabled"
        );
        log::error!(
            "To enable Discord notifications, set both DISCORD_BOT_TOKEN and DISCORD_CHANNEL_ID"
        );
        return None;
    }

    match DiscordNotifier::new() {
        Some(notifier) => {
            // Validate channel existence and permissions
            match notifier.validate_channel().await {
                Ok(_) => {
                    log::info!("Discord notifier initialized successfully");
                    Some(notifier)
                }
                Err(e) => {
                    log::error!("Discord channel validation failed: {}", e);
                    log::warn!("Discord notifications may not work correctly");
                    Some(notifier)
                }
            }
        }
        None => {
            log::error!("Discord notifier could not be initialized");
            None
        }
    }
}
