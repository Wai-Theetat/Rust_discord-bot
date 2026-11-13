use std::sync::Arc;

use serenity::async_trait;
use serenity::model::application::Interaction;
use serenity::model::gateway::Ready;
use serenity::model::id::GuildId;
use serenity::prelude::*;

use crate::commands;
use crate::commands::game::GameContext;
use crate::disabled::DisabledCommands;

pub struct Handler {
    pub guild_id: GuildId,
    pub games: GameContext,
    /// Commands switched off with /disable.
    pub disabled: Arc<DisabledCommands>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);

        match self.guild_id.set_commands(&ctx.http, commands::all()).await {
            Ok(list) => println!("Registered {} commands", list.len()),
            Err(err) => eprintln!("Could not register commands: {err}"),
        }

        // Keep the live status channel updated. `ready` fires again after a
        // reconnect; `start` only starts one loop.
        if let Some(launcher) = &self.games.launcher {
            self.games.board.start(ctx.http.clone(), launcher.clone());
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => {
                commands::handle(&ctx, &command, &self.games, &self.disabled).await;
            }

            // Suggestions while the user is still typing an option.
            // (None while /game is disabled.)
            Interaction::Autocomplete(interaction)
                if interaction.data.name == commands::game::NAME
                    && !self.disabled.is_disabled(commands::game::NAME) =>
            {
                commands::game::autocomplete(&ctx, &interaction, &self.games).await;
            }

            // The Delete/Cancel buttons of a /cleanup that was started before
            // /cleanup got disabled: block those too.
            Interaction::Component(component) => {
                if commands::cleanup::owns_button(&component.data.custom_id) && !self.disabled.is_disabled("cleanup") {
                    commands::cleanup::handle_button(&ctx, &component).await;
                }
            }

            _ => {}
        }
    }
}
