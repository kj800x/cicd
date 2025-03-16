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
                log::info!("DISCORD_BOT_TOKEN found");
                // Print a masked version of the token to confirm it's not empty or malformed
                let masked_token = if token.len() > 10 {
                    format!("{}...{}", &token[0..5], &token[token.len() - 5..])
                } else {
                    "[too short - check token]".to_string()
                };
                log::info!("Token format check (masked): {}", masked_token);
                token
            }
            Err(e) => {
                log::error!("DISCORD_BOT_TOKEN not set: {}", e);
                return None;
            }
        };

        let channel_id_str = match env::var("DISCORD_CHANNEL_ID") {
            Ok(id) => {
                log::info!("DISCORD_CHANNEL_ID found: {}", id);
                id
            }
            Err(e) => {
                log::error!("DISCORD_CHANNEL_ID not set: {}", e);
                return None;
            }
        };

        let channel_id = match channel_id_str.parse::<u64>() {
            Ok(id) => {
                log::info!("Parsed channel ID: {}", id);
                ChannelId::new(id)
            }
            Err(e) => {
                log::error!("DISCORD_CHANNEL_ID is not a valid ID: {}", e);
                return None;
            }
        };

        log::info!("Initializing Discord HTTP client");
        let http = Http::new(&token);

        // Validate the token by making a test API call
        let http = Arc::new(http);
        let http_clone = http.clone();
        tokio::spawn(async move {
            match http_clone.get_current_application_info().await {
                Ok(info) => {
                    log::info!("Discord bot validated, application name: {}", info.name);
                    log::info!("Discord bot ID: {}", info.id);
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
        log::info!(
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

        log::info!("Sending Discord message to channel ID: {}", self.channel_id);
        log::info!(
            "Current time before sending: {:?}",
            std::time::SystemTime::now()
        );

        match self.channel_id.send_message(&self.http, message).await {
            Ok(message) => {
                log::info!("Successfully sent Discord message, ID: {}", message.id);
                log::info!(
                    "Time after successful send: {:?}",
                    std::time::SystemTime::now()
                );

                // Store message ID for later updates
                let mut tracker = self.message_tracker.lock().await;
                tracker.insert(commit_sha.to_string(), message.id);
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to send Discord message: {}", e);
                log::error!("Time after failed send: {:?}", std::time::SystemTime::now());

                // Check if it's a permission issue
                if e.to_string().contains("Missing Permissions") {
                    log::error!("Bot lacks permissions to post in the channel");
                    log::error!("Required permissions: Send Messages, Embed Links");
                    log::error!("Bot URL for permissions: https://discord.com/oauth2/authorize?client_id=YOUR_CLIENT_ID&scope=bot&permissions=2048");
                } else if e.to_string().contains("Unknown Channel") {
                    log::error!("Channel not found - check if the channel ID is correct and the bot is in the server");
                } else if e.to_string().contains("Unauthorized") {
                    log::error!("Unauthorized - check if the bot token is correct");
                } else if e.to_string().contains("ratelimited") {
                    log::error!("Hit Discord rate limit - consider sending fewer messages");
                } else {
                    log::error!("Unexpected Discord API error: {}", e);
                    log::error!("Full error details: {:?}", e);
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
        log::info!(
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
            BuildStatus::Pending => ("🔄", "Build In Progress", Colour::BLUE),
            BuildStatus::None => ("⚠️", "Build Status Unknown", Colour::GOLD),
        };

        // Create builder function to generate a consistent embed
        let create_embed = || -> CreateEmbed {
            let mut embed = CreateEmbed::new()
                .title(format!(
                    "{} {}: {}/{}",
                    status_emoji, status_title, repo_owner, repo_name
                ))
                .description(format!(
                    "Build completed for commit `{}`",
                    &commit_sha[0..7]
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
            log::info!("Current message tracker state: {:?}", tracker);
            log::info!("Looking for commit SHA: {}", commit_sha);
            let id = tracker.get(commit_sha).cloned();
            if id.is_none() {
                log::warn!("No message ID found for commit SHA: {}", commit_sha);
            }
            id
        };

        if let Some(message_id) = message_id {
            log::info!("Updating existing Discord message, ID: {}", message_id);
            // Update existing message
            let edit_message = EditMessage::new().embed(create_embed());

            match self
                .channel_id
                .edit_message(&self.http, message_id, edit_message)
                .await
            {
                Ok(_) => {
                    log::info!("Successfully updated Discord message");
                    Ok(())
                }
                Err(e) => {
                    log::error!("Failed to update Discord message: {}", e);
                    log::error!("Full error details: {:?}", e);

                    if e.to_string().contains("Unknown Message") {
                        log::error!("Message not found - it may have been deleted");
                        // Try sending a new message instead
                        self.send_new_discord_message(create_embed()).await
                    } else {
                        Err(format!("Failed to update Discord message: {}", e))
                    }
                }
            }
        } else {
            log::info!(
                "No existing message found for commit {}, sending new message",
                commit_sha
            );
            self.send_new_discord_message(create_embed()).await
        }
    }

    // Helper method to send a new message with an embed
    async fn send_new_discord_message(&self, embed: CreateEmbed) -> Result<(), String> {
        log::info!(
            "Sending new Discord message to channel ID: {}",
            self.channel_id
        );
        log::debug!("Discord embed content: {:?}", embed);

        let message: CreateMessage = CreateMessage::new().add_embed(embed);

        match self.channel_id.send_message(&self.http, message).await {
            Ok(message) => {
                log::info!("Successfully sent new Discord message, ID: {}", message.id);
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to send new Discord message: {}", e);
                log::error!("Full error details: {:?}", e);

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
        log::info!("Validating Discord channel (ID: {})", self.channel_id);

        // Add detailed information about HTTP client state
        log::debug!("HTTP client type: {:?}", std::any::type_name::<Http>());

        // Test if the HTTP client is correctly initialized by making a simple API call
        match self.http.get_current_user().await {
            Ok(user) => log::info!(
                "HTTP client connection test successful - connected as user: {}",
                user.name
            ),
            Err(e) => log::error!(
                "HTTP client test failed - could not get current user: {}",
                e
            ),
        }

        match self.channel_id.to_channel(&self.http).await {
            Ok(channel) => {
                log::info!("Discord channel validated: {:?}", channel);

                // Try sending a test message with an embed and then delete it
                let test_embed: CreateEmbed = CreateEmbed::new()
                    .title("🔍 Bot Permissions Test")
                    .description("This message will be deleted automatically.")
                    .color(Colour::TEAL);

                let test_message: CreateMessage = CreateMessage::new().add_embed(test_embed);

                log::debug!(
                    "Sending test message to channel {} with embed",
                    self.channel_id
                );
                match self.channel_id.send_message(&self.http, test_message).await {
                    Ok(message) => {
                        log::info!(
                            "Successfully sent test message (ID: {}), will delete it now",
                            message.id
                        );
                        if let Err(e) = message.delete(&self.http).await {
                            log::warn!("Could not delete test message: {}", e);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        log::error!("Failed to send test message: {}", e);
                        // Check the specific error type
                        if e.to_string().contains("Missing Access") {
                            log::error!("Bot does not have access to the channel - check channel permissions");
                        } else if e.to_string().contains("Missing Permissions") {
                            log::error!("Bot lacks required permissions - needs at minimum: Send Messages, Embed Links");
                        } else if e.to_string().contains("Unknown Channel") {
                            log::error!("Channel ID is invalid or the bot cannot see this channel");
                        }
                        Err(format!("Failed to send test message: {}", e))
                    }
                }
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
}

// Add the Discord notifier to the application data
pub async fn setup_discord() -> Option<DiscordNotifier> {
    log::info!("Setting up Discord notifier");

    // Log all environment variables (without values) to see if they're being passed correctly
    log::info!(
        "Environment variables present: {}",
        env::vars().map(|(k, _)| k).collect::<Vec<_>>().join(", ")
    );

    // Check if Discord environment variables are present
    let has_token = env::var("DISCORD_BOT_TOKEN").is_ok();
    let has_channel = env::var("DISCORD_CHANNEL_ID").is_ok();

    log::info!("Discord environment variables status:");
    log::info!(
        "  DISCORD_BOT_TOKEN: {}",
        if has_token { "Present" } else { "MISSING" }
    );
    log::info!(
        "  DISCORD_CHANNEL_ID: {}",
        if has_channel { "Present" } else { "MISSING" }
    );

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
                    log::info!("Discord notifier initialized and validated successfully");
                    Some(notifier)
                }
                Err(e) => {
                    log::error!("Discord channel validation failed: {}", e);
                    // Try to get more detailed information about the channel
                    log::warn!("Attempting to retrieve more information about the channel...");

                    // Spawn a task to get additional channel information
                    let http = notifier.http.clone();
                    let channel_id = notifier.channel_id;
                    tokio::spawn(async move {
                        match channel_id.to_channel(&http).await {
                            Ok(channel) => log::info!("Channel info: {:?}", channel),
                            Err(e) => log::error!("Could not get channel info: {}", e),
                        }

                        // Check if the bot is in the server containing this channel
                        match http.get_guilds(None, None).await {
                            Ok(guilds) => {
                                if guilds.is_empty() {
                                    log::error!("Bot is not a member of any servers - it needs to be invited");
                                } else {
                                    log::info!(
                                        "Bot is in {} servers: {:?}",
                                        guilds.len(),
                                        guilds.iter().map(|g| g.name.clone()).collect::<Vec<_>>()
                                    );
                                }
                            }
                            Err(e) => log::error!("Could not get guilds: {}", e),
                        }

                        // Try to get current application info as a final test
                        match http.get_current_application_info().await {
                            Ok(info) => {
                                log::info!("Bot application info: {} (ID: {})", info.name, info.id)
                            }
                            Err(e) => log::error!("Could not get application info: {}", e),
                        }
                    });

                    // We still return the notifier but with a warning
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
