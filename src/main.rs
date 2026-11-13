mod commands;
mod handler;
mod math;
mod status_board;

use std::path::PathBuf;
use std::sync::Arc;

use bot_discord::games::client::LauncherClient;
use commands::game::GameContext;
use handler::Handler;
use serenity::model::id::{GuildId, RoleId, UserId};
use serenity::prelude::*;
use status_board::StatusBoard;

/// Where the game launcher listens, as seen from inside Docker.
/// (Running the bot directly on Windows? Set LAUNCHER_URL=http://127.0.0.1:7979.)
const DEFAULT_LAUNCHER_URL: &str = "http://host.docker.internal:7979";
/// Where the bot saves the status channel. Docker mounts ./data here (see docker-compose.yml).
const DEFAULT_DATA_DIR: &str = "data";

#[tokio::main]
async fn main() {
    // Running directly (cargo run): read .env. In Docker, env_file already set
    // the variables and there's no .env file inside the image, which is fine.
    let _ = dotenvy::dotenv();

    let token = std::env::var("DISCORD_TOKEN").expect("Token Not Found!");

    let intents = GatewayIntents::non_privileged();

    let guild_id = std::env::var("GUILD_ID")
        .expect("GUILD_ID Not Found!")
        .parse::<u64>()
        .expect("GUILD_ID must be a number");

    let guild_id = GuildId::new(guild_id);

    let data_dir = PathBuf::from(std::env::var("BOT_DATA_DIR").unwrap_or_else(|_| DEFAULT_DATA_DIR.to_string()));
    let games = GameContext {
        launcher: launcher_client().map(Arc::new),
        role: game_role(),
        owner: owner_id(),
        board: Arc::new(StatusBoard::load(&data_dir)),
    };

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler { guild_id, games })
        .await
        .expect("Client Error.");

    if let Err(msg) = client.start().await {
        eprintln!("Client error: {msg}")
    }
}

/// The connection to the game launcher. No LAUNCHER_TOKEN = /game is switched off.
fn launcher_client() -> Option<LauncherClient> {
    let Ok(token) = std::env::var("LAUNCHER_TOKEN") else {
        println!("LAUNCHER_TOKEN is not set: /game is switched off.");
        return None;
    };
    let url = std::env::var("LAUNCHER_URL").unwrap_or_else(|_| DEFAULT_LAUNCHER_URL.to_string());
    println!("/game will talk to the game launcher at {url}");
    Some(LauncherClient::new(url, token.trim()))
}

/// The one user allowed to run `/game action:setup`. Unset = nobody can.
fn owner_id() -> Option<UserId> {
    match std::env::var("OWNER_ID") {
        Ok(id) => {
            let id = id.trim().parse::<u64>().expect("OWNER_ID must be a number (your Discord user ID)");
            Some(UserId::new(id))
        }
        Err(_) => {
            println!("OWNER_ID is not set: nobody can run /game action:setup.");
            None
        }
    }
}

/// The role allowed to use /game. Unset = nobody can use it.
fn game_role() -> Option<RoleId> {
    match std::env::var("GAME_ROLE_ID") {
        Ok(id) => {
            let id = id.trim().parse::<u64>().expect("GAME_ROLE_ID must be a number");
            Some(RoleId::new(id))
        }
        Err(_) => {
            println!("GAME_ROLE_ID is not set: nobody can use /game.");
            None
        }
    }
}
