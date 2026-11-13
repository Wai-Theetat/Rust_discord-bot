use serenity::builder::{
    CreateAllowedMentions, CreateCommand, CreateInteractionResponse,
    CreateInteractionResponseMessage,
};
use serenity::model::application::CommandInteraction;
use serenity::prelude::*;

use crate::disabled::DisabledCommands;

pub mod benchmark;
pub mod cleanup;
pub mod echo;
pub mod factor;
pub mod game;
pub mod get_ip;
pub mod ping;
pub mod roll;
pub mod toggle;

/// Commands the owner can switch off with /disable. Everything except
/// /disable and /enable themselves (a test keeps this in sync with `all`).
pub const TOGGLEABLE: [&str; 8] = ["ping", "echo", "roll", "cleanup", "factor", "benchmark", "game", "get_ip"];

/// Every command definition sent to Discord on startup.
pub fn all() -> Vec<CreateCommand> {
    vec![
        ping::register(),
        echo::register(),
        roll::register(),
        cleanup::register(),
        factor::register(),
        benchmark::register(),
        game::register(),
        get_ip::register(),
        toggle::register_disable(),
        toggle::register_enable(),
    ]
}

/// Route a slash command to its implementation.
pub async fn handle(ctx: &Context, command: &CommandInteraction, games: &game::GameContext, disabled: &DisabledCommands) {
    let name = command.data.name.as_str();

    // Switched off with /disable: nobody may use it (the owner included).
    if disabled.is_disabled(name) {
        game::reply_ephemeral(ctx, command, &toggle::blocked_text(name)).await;
        return;
    }

    let reply = match name {
        // Commands that manage their own responses.
        "cleanup" => return cleanup::start(ctx, command).await,
        "factor" => return factor::start(ctx, command).await,
        "benchmark" => return benchmark::start(ctx, command).await,
        game::NAME => return game::handle(ctx, command, games).await,
        get_ip::NAME => return get_ip::handle(ctx, command, games).await,
        toggle::DISABLE | toggle::ENABLE => return toggle::handle(ctx, command, games, disabled).await,

        // Commands that return a single text reply.
        "ping" => ping::run(command),
        "echo" => echo::run(command),
        "roll" => roll::run(command),

        other => {
            eprintln!("Unknown command: {other}");
            return;
        }
    };

    // Never let user-supplied text ping @everyone, roles or users.
    let message = CreateInteractionResponseMessage::new()
        .content(reply)
        .allowed_mentions(CreateAllowedMentions::new());

    if let Err(error) = command
        .create_response(&ctx.http, CreateInteractionResponse::Message(message))
        .await
    {
        eprintln!("Could not reply to /{}: {error}", command.data.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggleable_matches_every_registered_command() {
        // If you add a command to `all()`, add it to TOGGLEABLE too (or this fails).
        let registered: Vec<String> = all()
            .into_iter()
            .map(|command| serde_json::to_value(command).unwrap()["name"].as_str().unwrap().to_string())
            .filter(|name| name != toggle::DISABLE && name != toggle::ENABLE)
            .collect();
        assert_eq!(registered, TOGGLEABLE);
    }
}
