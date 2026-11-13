//! `/get_ip`: the PC's public IP address, plus each game's full address.
//!
//! The bot asks a public "what's my IP" service. Requests from the Docker
//! container go out through the PC's internet connection, so the answer is
//! the PC's public IP (the one players connect to).
//!
//! It's your home IP, so only members with the GAME_ROLE_ID role may see it,
//! and the reply is ephemeral (only the user who asked sees it).

use std::net::IpAddr;
use std::time::Duration;

use bot_discord::games::launcher::GameInfo;
use serenity::builder::{CreateAllowedMentions, CreateCommand, EditInteractionResponse};
use serenity::model::application::CommandInteraction;
use serenity::prelude::*;

use super::game::{GameContext, check_access, escape_markdown, reply_ephemeral};

pub const NAME: &str = "get_ip";

/// Tried in order; the second is a fallback if the first is down.
const LOOKUP_SERVICES: [&str; 2] = ["https://api.ipify.org", "https://checkip.amazonaws.com"];
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn register() -> CreateCommand {
    CreateCommand::new(NAME).description("Show the game servers' public IP address.")
}

pub async fn handle(ctx: &Context, command: &CommandInteraction, games: &GameContext) {
    let roles = command.member.as_ref().map(|member| member.roles.as_slice()).unwrap_or(&[]);
    if let Err(message) = check_access(games.role, roles) {
        reply_ephemeral(ctx, command, &message).await;
        return;
    }

    if let Err(error) = command.defer_ephemeral(&ctx.http).await {
        eprintln!("Could not defer /get_ip: {error}");
        return;
    }

    let text = match public_ip().await {
        Ok(ip) => {
            // Ports come from the launcher. If it's offline, just show the IP.
            let servers = match &games.launcher {
                Some(launcher) => launcher.list().await.unwrap_or_default(),
                None => Vec::new(),
            };
            ip_text(ip, &servers)
        }
        Err(error) => format!("❌ {error}"),
    };

    let message = EditInteractionResponse::new()
        .content(text)
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(error) = command.edit_response(&ctx.http, message).await {
        eprintln!("Could not reply to /get_ip: {error}");
    }
}

/// Ask the lookup services until one gives a valid IP.
async fn public_ip() -> Result<IpAddr, String> {
    let client = reqwest::Client::builder()
        .timeout(LOOKUP_TIMEOUT)
        .build()
        .map_err(|error| format!("Could not set up the IP lookup: {error}"))?;

    for url in LOOKUP_SERVICES {
        let Ok(response) = client.get(url).send().await else { continue };
        let Ok(body) = response.text().await else { continue };
        if let Some(ip) = parse_ip(&body) {
            return Ok(ip);
        }
    }
    Err("Could not look up the public IP right now (both lookup services failed).".to_string())
}

/// The services answer with just the IP (plus maybe a newline). Anything that
/// isn't a valid IP is rejected, so odd replies never get shown.
fn parse_ip(body: &str) -> Option<IpAddr> {
    body.trim().parse().ok()
}

fn ip_text(ip: IpAddr, servers: &[GameInfo]) -> String {
    let mut text = format!("🌐 Server IP: `{ip}`");
    if !servers.is_empty() {
        text.push('\n');
        for server in servers {
            text.push_str(&format!("\n**{}**: `{}`", escape_markdown(&server.name), address(ip, server.port)));
        }
    }
    text.push_str("\n-# Only you can see this.");
    text
}

/// `1.2.3.4:25565`, or `[2001:db8::1]:25565` for IPv6.
fn address(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(ip) => format!("{ip}:{port}"),
        IpAddr::V6(ip) => format!("[{ip}]:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(name: &str, port: u16) -> GameInfo {
        GameInfo { name: name.into(), port, stop_timeout_secs: 30 }
    }

    #[test]
    fn parses_lookup_replies() {
        assert_eq!(parse_ip("203.0.113.7\n"), Some("203.0.113.7".parse().unwrap()));
        assert_eq!(parse_ip("  2001:db8::1 "), Some("2001:db8::1".parse().unwrap()));
        // An error page or anything else is not shown to users.
        assert_eq!(parse_ip("<html>Service Unavailable</html>"), None);
        assert_eq!(parse_ip(""), None);
    }

    #[test]
    fn lists_each_game_address() {
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let text = ip_text(ip, &[game("gtnh", 25565), game("terraria", 7777)]);
        assert_eq!(
            text,
            "🌐 Server IP: `203.0.113.7`\n\n**gtnh**: `203.0.113.7:25565`\n**terraria**: `203.0.113.7:7777`\n-# Only you can see this."
        );
    }

    #[test]
    fn works_without_the_launcher() {
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(ip_text(ip, &[]), "🌐 Server IP: `203.0.113.7`\n-# Only you can see this.");
    }

    #[test]
    fn ipv6_addresses_get_brackets() {
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(address(ip, 7777), "[2001:db8::1]:7777");
    }
}
