//! Discord notifications, for the events a person has to act on.
//!
//! The build notifications this replaces were removed for being noise;
//! only failed deploy runs post here. Messages go to `DISCORD_CHANNEL_ID`
//! through the REST API with the bot token in `DISCORD_BOT_TOKEN`. Without
//! either, notifications are logged and skipped.

/// Where links in notifications point.
pub const CICD_URL: &str = "https://cicd.home.coolkev.com";

/// Discord rejects messages longer than this.
const MAX_LEN: usize = 2000;

/// Post `message` to the channel. Best effort: failures are logged, never
/// returned, since nothing should wait on a notification.
pub async fn notify(message: &str) {
    let (Ok(token), Ok(channel)) = (
        std::env::var("DISCORD_BOT_TOKEN"),
        std::env::var("DISCORD_CHANNEL_ID"),
    ) else {
        log::warn!("Discord is not configured; not sending: {}", message);
        return;
    };
    let content = truncate(message);
    let result = reqwest::Client::new()
        .post(format!(
            "https://discord.com/api/v10/channels/{}/messages",
            channel.trim()
        ))
        .header("Authorization", format!("Bot {}", token.trim()))
        .json(&serde_json::json!({ "content": content }))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status);
    match result {
        Ok(_) => log::info!("Sent Discord notification"),
        Err(e) => log::error!("Discord notification failed: {}", e),
    }
}

fn truncate(message: &str) -> String {
    if message.chars().count() <= MAX_LEN {
        return message.to_string();
    }
    let mut cut: String = message.chars().take(MAX_LEN - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_messages_are_cut_to_the_limit() {
        assert_eq!(truncate("short"), "short");
        let long = "x".repeat(MAX_LEN + 10);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), MAX_LEN);
        assert!(cut.ends_with('…'));
    }
}
