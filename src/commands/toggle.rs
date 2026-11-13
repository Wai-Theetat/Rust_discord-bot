//! `/disable command:<name>` and `/enable command:<name>`: switch commands
//! off and on. Owner only (OWNER_ID). A disabled command is blocked for
//! everyone, the owner included, until it's enabled again.
//!
//! `/disable` with no `command` lists what's currently off.

use serenity::builder::{CreateCommand, CreateCommandOption};
use serenity::model::application::{CommandInteraction, CommandOptionType};
use serenity::prelude::*;

use super::TOGGLEABLE;
use super::game::{GameContext, check_owner, reply_ephemeral};
use crate::disabled::DisabledCommands;

pub const DISABLE: &str = "disable";
pub const ENABLE: &str = "enable";

pub fn register_disable() -> CreateCommand {
    CreateCommand::new(DISABLE)
        .description("Owner only: switch a command off for everyone.")
        .add_option(command_option("Command to switch off (leave empty to list disabled ones)", false))
}

pub fn register_enable() -> CreateCommand {
    CreateCommand::new(ENABLE)
        .description("Owner only: switch a command back on.")
        .add_option(command_option("Command to switch back on", true))
}

/// A pick-list of every command that can be switched off.
/// /disable and /enable aren't in it, so the owner can't lock themselves out.
fn command_option(description: &str, required: bool) -> CreateCommandOption {
    let mut option = CreateCommandOption::new(CommandOptionType::String, "command", description).required(required);
    for name in TOGGLEABLE {
        option = option.add_string_choice(format!("/{name}"), name);
    }
    option
}

pub async fn handle(ctx: &Context, command: &CommandInteraction, games: &GameContext, disabled: &DisabledCommands) {
    if let Err(message) = check_owner(games.owner, command.user.id) {
        reply_ephemeral(ctx, command, &message).await;
        return;
    }

    let disable = command.data.name == DISABLE;
    let target = command
        .data
        .options
        .iter()
        .find(|option| option.name == "command")
        .and_then(|option| option.value.as_str());

    let text = match target {
        None => list_text(&disabled.list()),
        Some(name) => toggle(disabled, name, disable),
    };
    reply_ephemeral(ctx, command, &text).await;
}

fn toggle(disabled: &DisabledCommands, name: &str, disable: bool) -> String {
    // The choices only offer valid names, but a client could send anything.
    if !TOGGLEABLE.contains(&name) {
        return format!("`{}` isn't a command that can be switched off.", name.replace('`', "'"));
    }
    match (disabled.set(name, disable), disable) {
        (Ok(true), true) => format!("🚫 `/{name}` is now **disabled** for everyone. Use `/enable` to turn it back on."),
        (Ok(true), false) => format!("✅ `/{name}` is **enabled** again."),
        (Ok(false), true) => format!("`/{name}` was already disabled."),
        (Ok(false), false) => format!("`/{name}` wasn't disabled."),
        (Err(error), _) => format!("❌ Could not save the change: {error}"),
    }
}

fn list_text(names: &[String]) -> String {
    if names.is_empty() {
        "All commands are enabled.".to_string()
    } else {
        let names: Vec<String> = names.iter().map(|name| format!("`/{name}`")).collect();
        format!("🚫 Disabled: {}", names.join(", "))
    }
}

/// What users see when they run a disabled command.
pub fn blocked_text(name: &str) -> String {
    format!("🚫 `/{name}` is disabled right now.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> DisabledCommands {
        let dir = std::env::temp_dir().join(format!("bot_discord_toggle_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DisabledCommands::load(&dir)
    }

    #[test]
    fn toggling_and_listing() {
        let disabled = temp("toggle");
        assert_eq!(list_text(&disabled.list()), "All commands are enabled.");

        assert!(toggle(&disabled, "game", true).contains("now **disabled**"));
        assert!(toggle(&disabled, "game", true).contains("already disabled"));
        assert!(toggle(&disabled, "roll", true).contains("now **disabled**"));
        assert_eq!(list_text(&disabled.list()), "🚫 Disabled: `/game`, `/roll`");

        assert!(toggle(&disabled, "game", false).contains("enabled** again"));
        assert!(toggle(&disabled, "game", false).contains("wasn't disabled"));
        assert_eq!(disabled.list(), ["roll"]);
    }

    #[test]
    fn cannot_lock_yourself_out_or_use_unknown_names() {
        let disabled = temp("lockout");
        // /disable and /enable themselves can never be switched off.
        assert!(toggle(&disabled, DISABLE, true).contains("isn't a command"));
        assert!(toggle(&disabled, ENABLE, true).contains("isn't a command"));
        assert!(toggle(&disabled, "nuke", true).contains("isn't a command"));
        assert!(disabled.list().is_empty());
    }

    #[test]
    fn pick_list_has_every_switchable_command_and_not_itself() {
        let json = serde_json::to_value(register_disable()).unwrap();
        let choices: Vec<&str> = json["options"][0]["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap())
            .collect();
        assert_eq!(choices, TOGGLEABLE);
        assert!(!choices.contains(&DISABLE) && !choices.contains(&ENABLE));
        assert_ne!(json["options"][0]["required"], true, "/disable alone lists");

        let enable = serde_json::to_value(register_enable()).unwrap();
        assert_eq!(enable["options"][0]["required"], true);
    }
}
