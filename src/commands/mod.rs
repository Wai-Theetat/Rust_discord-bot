use serenity::builder::{
    CreateAllowedMentions, CreateCommand, CreateInteractionResponse,
    CreateInteractionResponseMessage,
};
use serenity::model::application::CommandInteraction;
use serenity::prelude::*;

pub mod benchmark;
pub mod cleanup;
pub mod echo;
pub mod factor;
pub mod game;
pub mod get_ip;
pub mod ping;
pub mod roll;

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
    ]
}

/// Route a slash command to its implementation.
pub async fn handle(ctx: &Context, command: &CommandInteraction, games: &game::GameContext) {
    let reply = match command.data.name.as_str() {
        // Commands that manage their own responses.
        "cleanup" => return cleanup::start(ctx, command).await,
        "factor" => return factor::start(ctx, command).await,
        "benchmark" => return benchmark::start(ctx, command).await,
        game::NAME => return game::handle(ctx, command, games).await,
        get_ip::NAME => return get_ip::handle(ctx, command, games).await,

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
