//! `/game action:<start|stop|restart|check|setup> name:<server>`
//!
//! The bot runs in Docker and can't start Windows programs itself, so this
//! command asks the game launcher (src/bin/launcher.rs, running on the PC) over HTTP.
//!
//! Only members with the role in GAME_ROLE_ID may use it. Every reply is
//! ephemeral (only the user who ran it sees it). The public view is the status
//! channel picked with `action:setup`.

use std::sync::Arc;
use std::time::Duration;

use bot_discord::games::launcher::{GameInfo, GameState, Resources};
use bot_discord::games::mc_ping::PingInfo;
use bot_discord::games::client::{LauncherClient, LauncherError};
use bot_discord::games::manager::{StartOutcome, Status, StopOutcome};
use serenity::builder::{
    CreateAllowedMentions, CreateAutocompleteResponse, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, EditInteractionResponse,
};
use serenity::model::application::{CommandInteraction, CommandOptionType};
use serenity::model::id::{RoleId, UserId};
use serenity::prelude::*;

use crate::status_board::StatusBoard;

pub const NAME: &str = "game";

/// The interaction token lasts 15 minutes; stop editing a bit before that.
const READY_TIMEOUT: Duration = Duration::from_secs(14 * 60);
const READY_POLL: Duration = Duration::from_secs(2);
/// How much of the console to show after a crash (keeps us under Discord's 2000 chars).
const LOG_LINES_SHOWN: usize = 8;
const LOG_LINE_MAX_CHARS: usize = 150;
/// Discord shows at most 25 autocomplete choices.
const MAX_CHOICES: usize = 25;

/// Everything `/game` needs, created once at startup.
pub struct GameContext {
    /// None = LAUNCHER_TOKEN not set, so /game is switched off.
    pub launcher: Option<Arc<LauncherClient>>,
    /// Members need this role. None = GAME_ROLE_ID not set, so nobody may use it.
    pub role: Option<RoleId>,
    /// The only user who may run `action:setup`. None = OWNER_ID not set, so nobody may.
    pub owner: Option<UserId>,
    /// The live status channel (`/game action:setup`).
    pub board: Arc<StatusBoard>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Start,
    Stop,
    Restart,
    Check,
    Setup,
    StopAll,
}

impl Action {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            "check" => Some(Self::Check),
            "setup" => Some(Self::Setup),
            "stopall" => Some(Self::StopAll),
            _ => None,
        }
    }
}

// ---------- registration ----------

pub fn register() -> CreateCommand {
    CreateCommand::new(NAME)
        .description("Start, stop or check a game server.")
        // Discord lists required options first, so `action` comes first.
        .add_option(
            CreateCommandOption::new(CommandOptionType::String, "action", "What to do")
                .required(true)
                .add_string_choice("start", "start")
                .add_string_choice("stop", "stop")
                .add_string_choice("restart", "restart")
                .add_string_choice("check", "check")
                .add_string_choice("setup", "setup")
                .add_string_choice("stop all (only if nobody is online)", "stopall"),
        )
        // Not required, because `setup` doesn't need it. Checked in `handle`.
        .add_option(
            CreateCommandOption::new(CommandOptionType::String, "name", "Which server")
                .required(false)
                .set_autocomplete(true),
        )
}

// ---------- autocomplete ----------

/// Suggest server names while the user types the `name` option.
/// If the launcher is down, it just suggests nothing.
pub async fn autocomplete(ctx: &Context, interaction: &CommandInteraction, games: &GameContext) {
    let typed = interaction
        .data
        .autocomplete()
        .map(|option| option.value)
        .unwrap_or("");

    let names = match &games.launcher {
        Some(launcher) => game_names(launcher).await,
        None => Vec::new(),
    };
    let names: Vec<&str> = names.iter().map(String::as_str).collect();

    let mut response = CreateAutocompleteResponse::new();
    for name in matching_names(&names, typed) {
        response = response.add_string_choice(name, name);
    }

    if let Err(error) = interaction
        .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response))
        .await
    {
        eprintln!("Could not send /game autocomplete: {error}");
    }
}

/// All game names from the launcher, or none if it can't be reached.
async fn game_names(launcher: &LauncherClient) -> Vec<String> {
    match launcher.list().await {
        Ok(games) => games.into_iter().map(|game| game.name).collect(),
        Err(_) => Vec::new(),
    }
}

fn matching_names<'a>(names: &[&'a str], typed: &str) -> Vec<&'a str> {
    let typed = typed.to_lowercase();
    names
        .iter()
        .filter(|name| name.to_lowercase().contains(&typed))
        .take(MAX_CHOICES)
        .copied()
        .collect()
}

// ---------- the command ----------

pub async fn handle(ctx: &Context, command: &CommandInteraction, games: &GameContext) {
    let name = option_str(command, "name").unwrap_or("");
    let Some(action) = option_str(command, "action").and_then(Action::parse) else {
        reply_ephemeral(ctx, command, "Unknown action. Use start, stop, restart, check, setup or stop all.").await;
        return;
    };

    // Access first. Setup is owner-only (by user id, role not needed);
    // everything else needs the role. No member = used outside a server.
    let access = if action == Action::Setup {
        check_owner(games.owner, command.user.id)
    } else {
        let roles = command.member.as_ref().map(|member| member.roles.as_slice()).unwrap_or(&[]);
        check_access(games.role, roles)
    };
    if let Err(message) = access {
        reply_ephemeral(ctx, command, &message).await;
        return;
    }

    let Some(launcher) = &games.launcher else {
        reply_ephemeral(ctx, command, "`/game` isn't set up yet: the bot owner needs to set LAUNCHER_TOKEN.").await;
        return;
    };

    if action == Action::Setup {
        setup(ctx, command, launcher, &games.board).await;
        return;
    }
    if action == Action::StopAll {
        stop_all(ctx, command, launcher).await;
        return;
    }
    if name.is_empty() {
        reply_ephemeral(ctx, command, "Pick a server with the `name` option.").await;
        return;
    }

    // Autocomplete only suggests; users can still type any name. Asking the
    // launcher first also tells us early if it's offline.
    let info = match launcher.state(name).await {
        Ok(state) => state.info,
        Err(LauncherError::NotFound) => {
            let names = game_names(launcher).await;
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            reply_ephemeral(ctx, command, &unknown_game_text(name, &names)).await;
            return;
        }
        Err(error) => {
            reply_ephemeral(ctx, command, &format!("❌ {error}")).await;
            return;
        }
    };

    // Starting or stopping can take minutes. Defer = "Bot is thinking...",
    // then we edit that message as things progress. Ephemeral: only the user
    // who ran the command sees it (everyone else watches the status channel).
    if let Err(error) = command.defer_ephemeral(&ctx.http).await {
        eprintln!("Could not defer /game: {error}");
        return;
    }
    let reply = Reply { ctx, command };

    match action {
        Action::Check => match launcher.state(&info.name).await {
            Ok(state) => {
                let mut text = status_text(&info, &state.status);
                if let Some(ping) = &state.ping {
                    text.push('\n');
                    text.push_str(&ping_text(ping));
                }
                if let Some(resources) = &state.resources {
                    text.push('\n');
                    text.push_str(&resources_text(resources));
                }
                reply.edit(text).await;
            }
            Err(error) => reply.edit(format!("❌ {error}")).await,
        },
        Action::Start => start_and_wait(&reply, launcher, &info, "").await,
        Action::Stop => {
            let text = stop_and_describe(&reply, launcher, &info).await;
            reply.edit(text).await;
        }
        Action::Setup | Action::StopAll => unreachable!("handled above"),
        Action::Restart => {
            if is_stopped(launcher, &info).await {
                start_and_wait(&reply, launcher, &info, "").await;
                return;
            }
            let stopped = stop_and_describe(&reply, launcher, &info).await;
            if !is_stopped(launcher, &info).await {
                // Stopping failed, so don't try to start a second copy.
                reply.edit(stopped).await;
                return;
            }
            start_and_wait(&reply, launcher, &info, &format!("{stopped}\n")).await;
        }
    }
}

/// `/game action:setup`: make this channel the live status channel.
/// Only the OWNER_ID user may run it (checked in `handle`).
async fn setup(ctx: &Context, command: &CommandInteraction, launcher: &LauncherClient, board: &StatusBoard) {
    // Posting the messages takes a moment. Only the user sees this reply.
    if let Err(error) = command.defer_ephemeral(&ctx.http).await {
        eprintln!("Could not defer /game action:setup: {error}");
        return;
    }
    let reply = Reply { ctx, command };
    match board.set_channel(command.channel_id, &ctx.http, launcher).await {
        Ok(()) => reply.edit("✅ This channel now shows live game server status, and crash alerts.").await,
        Err(error) => reply.edit(format!("⚠️ Saved this channel, but something went wrong: {error}")).await,
    }
}

async fn is_stopped(launcher: &LauncherClient, info: &GameInfo) -> bool {
    matches!(launcher.state(&info.name).await, Ok(state) if matches!(state.status, Status::Stopped { .. }))
}

/// Start the server, then keep editing the reply until it's ready (or died).
async fn start_and_wait(reply: &Reply<'_>, launcher: &LauncherClient, info: &GameInfo, prefix: &str) {
    let name = escape_markdown(&info.name);
    match launcher.start(&info.name).await {
        Err(error) => reply.edit(format!("{prefix}❌ Could not start **{name}**: {error}")).await,
        Ok(StartOutcome::AlreadyRunning(status)) => {
            reply
                .edit(format!("{prefix}**{name}** is already running.\n{}", status_text(info, &status)))
                .await;
        }
        Ok(StartOutcome::Started) => {
            reply
                .edit(format!("{prefix}⏳ Starting **{name}**... I'll update this message when it's ready."))
                .await;

            let text = match wait_until_settled(launcher, info).await {
                Ok(status @ Status::Running { .. }) => {
                    format!("✅ **{name}** is ready!\n{}", status_text(info, &status))
                }
                Ok(status @ Status::Stopped { .. }) => {
                    format!("❌ **{name}** stopped while starting.\n{}", status_text(info, &status))
                }
                Ok(Status::Starting { .. }) => format!(
                    "⏳ **{name}** is still starting after {}. Use `/game check` later.",
                    format_duration(READY_TIMEOUT)
                ),
                Err(error) => format!("⚠️ Lost track of **{name}** while it was starting: {error}"),
            };
            reply.edit(format!("{prefix}{text}")).await;
        }
    }
}

/// Poll until the server is no longer "starting", or we run out of time.
/// A failed poll (launcher briefly busy) is retried; only the last one counts.
async fn wait_until_settled(launcher: &LauncherClient, info: &GameInfo) -> Result<Status, LauncherError> {
    let started = tokio::time::Instant::now();
    loop {
        let result = launcher.state(&info.name).await.map(|state| state.status);
        let settled = matches!(&result, Ok(status) if !matches!(status, Status::Starting { .. }));
        if settled || started.elapsed() >= READY_TIMEOUT {
            return result;
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// Stop the server and return a line describing what happened.
async fn stop_and_describe(reply: &Reply<'_>, launcher: &LauncherClient, info: &GameInfo) -> String {
    let name = escape_markdown(&info.name);
    if is_stopped(launcher, info).await {
        return format!("**{name}** isn't running.");
    }

    let timeout = format_duration(Duration::from_secs(info.stop_timeout_secs));
    reply
        .edit(format!("⏳ Stopping **{name}** and saving the world... (can take up to {timeout})"))
        .await;

    stop_result_text(info, launcher.stop(&info.name, info.stop_timeout_secs).await)
}

/// One line describing how a stop went.
fn stop_result_text(info: &GameInfo, result: Result<StopOutcome, LauncherError>) -> String {
    let name = escape_markdown(&info.name);
    let timeout = format_duration(Duration::from_secs(info.stop_timeout_secs));
    match result {
        Ok(StopOutcome::NotRunning) => format!("**{name}** isn't running."),
        Ok(StopOutcome::Stopped) => format!("✅ **{name}** stopped. World saved."),
        Ok(StopOutcome::ForceKilled) => format!(
            "⚠️ **{name}** didn't stop within {timeout}, so it was force-killed. The world may not have saved."
        ),
        Err(error) => format!("❌ Could not stop **{name}**: {error}"),
    }
}

/// `action:stopall`: save and stop every running server, but only when
/// nobody is playing on any of them.
async fn stop_all(ctx: &Context, command: &CommandInteraction, launcher: &Arc<LauncherClient>) {
    if let Err(error) = command.defer_ephemeral(&ctx.http).await {
        eprintln!("Could not defer /game action:stopall: {error}");
        return;
    }
    let reply = Reply { ctx, command };

    // Check every game first. One we can't read blocks everything: "couldn't
    // check" must never count as "nobody online".
    let games = match launcher.list().await {
        Ok(games) => games,
        Err(error) => return reply.edit(format!("❌ {error}")).await,
    };
    let mut states = Vec::with_capacity(games.len());
    for game in &games {
        match launcher.state(&game.name).await {
            Ok(state) => states.push(state),
            Err(error) => {
                let name = escape_markdown(&game.name);
                return reply.edit(format!("❌ Couldn't check **{name}**, so nothing was stopped: {error}")).await;
            }
        }
    }

    let to_stop = match stop_all_plan(&states) {
        Err(refusal) => return reply.edit(refusal).await,
        Ok(to_stop) if to_stop.is_empty() => return reply.edit("Nothing is running.").await,
        Ok(to_stop) => to_stop,
    };

    let names: Vec<String> = to_stop.iter().map(|info| format!("**{}**", escape_markdown(&info.name))).collect();
    reply
        .edit(format!("⏳ Stopping {} and saving... (GTNH can take a few minutes)", names.join(", ")))
        .await;

    // All at the same time, so a slow GTNH save doesn't hold up Terraria.
    let mut tasks = tokio::task::JoinSet::new();
    for info in to_stop {
        let launcher = Arc::clone(launcher);
        tasks.spawn(async move {
            let result = launcher.stop(&info.name, info.stop_timeout_secs).await;
            stop_result_text(&info, result)
        });
    }
    let mut lines = Vec::new();
    while let Some(result) = tasks.join_next().await {
        lines.push(result.unwrap_or_else(|error| format!("❌ A stop task failed: {error}")));
    }
    lines.sort();
    reply.edit(lines.join("\n")).await;
}

/// Which servers stop-all would stop, or why it refuses.
/// Refuses if anyone is on any running server. Servers still starting have
/// nobody on them yet, so they can be stopped.
fn stop_all_plan(states: &[GameState]) -> Result<Vec<GameInfo>, String> {
    let occupied: Vec<String> = states
        .iter()
        .filter_map(|state| match &state.status {
            Status::Running { players, .. } if !players.is_empty() => {
                let names: Vec<String> = players.iter().map(|player| escape_markdown(player)).collect();
                Some(format!("**{}**: {}", escape_markdown(&state.info.name), names.join(", ")))
            }
            _ => None,
        })
        .collect();
    if !occupied.is_empty() {
        return Err(format!(
            "❌ Not stopping anything: people are still online.\n{}",
            occupied.join("\n")
        ));
    }

    Ok(states
        .iter()
        .filter(|state| !matches!(state.status, Status::Stopped { .. }))
        .map(|state| state.info.clone())
        .collect())
}

// ---------- replying ----------

/// Edits the deferred reply. Mentions are always disabled: the text can contain
/// server log lines, and those can contain things players typed (like @everyone).
struct Reply<'a> {
    ctx: &'a Context,
    command: &'a CommandInteraction,
}

impl Reply<'_> {
    async fn edit(&self, text: impl Into<String>) {
        let message = EditInteractionResponse::new()
            .content(text)
            .allowed_mentions(CreateAllowedMentions::new());
        if let Err(error) = self.command.edit_response(&self.ctx.http, message).await {
            eprintln!("Could not edit /game reply: {error}");
        }
    }
}

pub(crate) async fn reply_ephemeral(ctx: &Context, command: &CommandInteraction, text: &str) {
    let message = CreateInteractionResponseMessage::new()
        .content(text)
        .ephemeral(true)
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(error) = command
        .create_response(&ctx.http, CreateInteractionResponse::Message(message))
        .await
    {
        eprintln!("Could not reply to /game: {error}");
    }
}

fn option_str<'a>(command: &'a CommandInteraction, name: &str) -> Option<&'a str> {
    command
        .data
        .options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| option.value.as_str())
}

// ---------- pure helpers (tested below) ----------

/// For owner-only actions (setup, /disable, /enable). Unset OWNER_ID = nobody.
pub(crate) fn check_owner(owner: Option<UserId>, user: UserId) -> Result<(), String> {
    match owner {
        // Not configured = nobody (fail closed), same as GAME_ROLE_ID.
        None => Err("This isn't set up yet: the bot owner needs to set OWNER_ID.".to_string()),
        Some(owner) if owner == user => Ok(()),
        Some(_) => Err("Only the bot owner can do this.".to_string()),
    }
}

pub(crate) fn check_access(required: Option<RoleId>, member_roles: &[RoleId]) -> Result<(), String> {
    match required {
        None => Err("`/game` isn't set up yet: the bot owner needs to set GAME_ROLE_ID.".to_string()),
        Some(role) if member_roles.contains(&role) => Ok(()),
        // <@&id> shows the role's name. Mentions are disabled, so nobody gets pinged.
        Some(role) => Err(format!("You need the <@&{role}> role to use `/game`.")),
    }
}

fn unknown_game_text(typed: &str, names: &[&str]) -> String {
    // The user typed this, so keep it from breaking out of the `code` span.
    let typed: String = typed.replace('`', "'").chars().take(50).collect();
    if names.is_empty() {
        format!("Unknown game `{typed}`. No game servers are configured.")
    } else {
        format!("Unknown game `{typed}`. Available: {}", names.join(", "))
    }
}

fn status_text(info: &GameInfo, status: &Status) -> String {
    format_status(&info.name, info.port, status)
}

pub(crate) fn format_status(name: &str, port: u16, status: &Status) -> String {
    let name = escape_markdown(name);
    match status {
        Status::Running { uptime, players } => format!(
            "🟢 **{name}** is online · port `{port}` · up {}\n{}",
            format_duration(*uptime),
            players_line(players)
        ),
        Status::Starting { elapsed } => {
            format!("🟡 **{name}** is starting ({} so far)", format_duration(*elapsed))
        }
        Status::Stopped { last_exit: Some(exit) } if exit.crashed => {
            let code = exit.code.map(|code| format!(" (exit code {code})")).unwrap_or_default();
            format!(
                "🔴 **{name}** is offline\n⚠️ It crashed last time{code}. Last lines:\n{}",
                log_block(&exit.recent_lines)
            )
        }
        Status::Stopped { .. } => format!("🔴 **{name}** is offline"),
    }
}

fn players_line(players: &[String]) -> String {
    if players.is_empty() {
        return "No players online".to_string();
    }
    let names: Vec<String> = players.iter().map(|player| escape_markdown(player)).collect();
    format!("Players ({}): {}", players.len(), names.join(", "))
}

/// Last few console lines in a code block. A line containing ``` would end the
/// block early, so those are swapped for '''.
fn log_block(lines: &[String]) -> String {
    let start = lines.len().saturating_sub(LOG_LINES_SHOWN);
    let shown: Vec<String> = lines[start..]
        .iter()
        .map(|line| line.replace("```", "'''").chars().take(LOG_LINE_MAX_CHARS).collect())
        .collect();
    format!("```text\n{}\n```", shown.join("\n"))
}

/// Discord markdown: `Mr_Bob_` would otherwise turn into italics.
pub(crate) fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Minecraft's own view, for `check`: latency, slots, version, MOTD.
fn ping_text(ping: &PingInfo) -> String {
    let mut text = format!(
        "📶 Ping {} ms · Slots {}/{} · Version {}",
        ping.latency_ms,
        ping.online,
        ping.max,
        escape_markdown(&ping.version)
    );
    if !ping.motd.is_empty() {
        text.push_str(&format!("\n💬 {}", escape_markdown(&ping.motd)));
    }
    text
}

/// RAM, CPU and start time for `check`.
fn resources_text(r: &Resources) -> String {
    let pc_percent = if r.pc_memory_total > 0 {
        r.pc_memory_used as f64 / r.pc_memory_total as f64 * 100.0
    } else {
        0.0
    };
    // <t:..:f> / <t:..:R> are Discord timestamps: each viewer sees their own
    // time zone, and "2 hours ago" updates by itself.
    format!(
        "🧠 Server RAM: **{}** · ⚙️ CPU: **{:.0}%**\n\
         🖥️ PC RAM: {} / {} used ({pc_percent:.0}%), {} free\n\
         🕒 Started <t:{}:f> (<t:{}:R>)",
        format_bytes(r.server_memory),
        r.server_cpu_percent,
        format_bytes(r.pc_memory_used),
        format_bytes(r.pc_memory_total),
        format_bytes(r.pc_memory_total.saturating_sub(r.pc_memory_used)),
        r.started_unix,
        r.started_unix,
    )
}

/// 6.2 GB, 512 MB, ... (1 GB = 1024³ bytes, like Task Manager).
fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_discord::games::manager::LastExit;

    #[test]
    fn parses_actions() {
        assert_eq!(Action::parse("start"), Some(Action::Start));
        assert_eq!(Action::parse("check"), Some(Action::Check));
        assert_eq!(Action::parse("setup"), Some(Action::Setup));
        assert_eq!(Action::parse("delete-world"), None);
    }

    #[test]
    fn registered_with_action_and_name_options() {
        // What Discord receives when the bot registers /game.
        let json = serde_json::to_value(register()).unwrap();
        let options = json["options"].as_array().unwrap();
        assert_eq!(options.len(), 2);

        assert_eq!(options[0]["name"], "action");
        assert_eq!(options[0]["required"], true);
        let choices: Vec<&str> = options[0]["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap())
            .collect();
        assert_eq!(choices, ["start", "stop", "restart", "check", "setup", "stopall"]);
        // Every choice must be something `handle` understands.
        assert!(choices.iter().all(|value| Action::parse(value).is_some()));

        assert_eq!(options[1]["name"], "name");
        assert_eq!(options[1]["autocomplete"], true);
        assert_ne!(options[1]["required"], true, "setup has no name, so it can't be required");
    }

    fn state(name: &str, status: Status) -> GameState {
        GameState {
            info: GameInfo { name: name.into(), port: 1, stop_timeout_secs: 30 },
            status,
            resources: None,
            ping: None,
        }
    }

    fn online(players: &[&str]) -> Status {
        Status::Running { uptime: Duration::from_secs(60), players: players.iter().map(|p| p.to_string()).collect() }
    }

    fn names(plan: Result<Vec<GameInfo>, String>) -> Vec<String> {
        plan.unwrap().into_iter().map(|info| info.name).collect()
    }

    #[test]
    fn stop_all_stops_running_and_starting_servers_when_empty() {
        let states = [
            state("gtnh", online(&[])),
            state("terraria", Status::Starting { elapsed: Duration::from_secs(5) }),
            state("old", Status::Stopped { last_exit: None }),
        ];
        assert_eq!(names(stop_all_plan(&states)), ["gtnh", "terraria"]);
    }

    #[test]
    fn stop_all_refuses_if_anyone_is_online_anywhere() {
        let states = [state("gtnh", online(&[])), state("terraria", online(&["Steve", "Mr_Bob"]))];
        let refusal = stop_all_plan(&states).unwrap_err();
        assert!(refusal.contains("Not stopping anything"), "{refusal}");
        // Says who's on where, so you know why.
        assert!(refusal.contains("**terraria**: Steve, Mr\\_Bob"), "{refusal}");
        assert!(!refusal.contains("gtnh"), "{refusal}");
    }

    #[test]
    fn stop_all_with_nothing_running() {
        assert!(names(stop_all_plan(&[state("gtnh", Status::Stopped { last_exit: None })])).is_empty());
        assert!(names(stop_all_plan(&[])).is_empty());
    }

    #[test]
    fn only_the_owner_can_setup() {
        let owner = Some(UserId::new(111111111111111111));
        assert!(check_owner(owner, UserId::new(111111111111111111)).is_ok());
        assert!(check_owner(owner, UserId::new(222222222222222222)).is_err());
        assert!(check_owner(owner, UserId::new(1)).is_err());
        // OWNER_ID missing: nobody, not everybody.
        assert!(check_owner(None, UserId::new(111111111111111111)).is_err());
    }

    #[test]
    fn access_needs_the_role() {
        let role = RoleId::new(42);
        assert!(check_access(Some(role), &[RoleId::new(1), role]).is_ok());

        let denied = check_access(Some(role), &[RoleId::new(1)]).unwrap_err();
        assert!(denied.contains("<@&42>"), "{denied}");

        // Not configured = nobody gets in (fail closed), not everybody.
        assert!(check_access(None, &[role]).is_err());
        assert!(check_access(Some(role), &[]).is_err());
    }

    #[test]
    fn autocomplete_matches_anywhere_ignoring_case() {
        let names = ["gtnh", "gtnh-creative", "terraria"];
        assert_eq!(matching_names(&names, ""), names);
        assert_eq!(matching_names(&names, "GT"), ["gtnh", "gtnh-creative"]);
        assert_eq!(matching_names(&names, "rar"), ["terraria"]);
        assert!(matching_names(&names, "valheim").is_empty());
    }

    #[test]
    fn unknown_game_lists_choices_and_neutralizes_backticks() {
        let text = unknown_game_text("a`b", &["gtnh", "terraria"]);
        assert_eq!(text, "Unknown game `a'b`. Available: gtnh, terraria");
    }

    #[test]
    fn ping_line() {
        let ping = PingInfo { latency_ms: 3, online: 1, max: 20, version: "1.7.10".into(), motd: "GT_New_Horizons".into() };
        assert_eq!(ping_text(&ping), "📶 Ping 3 ms · Slots 1/20 · Version 1.7.10\n💬 GT\\_New\\_Horizons");

        let no_motd = PingInfo { motd: String::new(), ..ping };
        assert_eq!(ping_text(&no_motd), "📶 Ping 3 ms · Slots 1/20 · Version 1.7.10");
    }

    #[test]
    fn bytes_like_task_manager() {
        assert_eq!(format_bytes(512 * 1024 * 1024), "512 MB");
        assert_eq!(format_bytes(6 * 1024 * 1024 * 1024 + 200 * 1024 * 1024), "6.2 GB");
        assert_eq!(format_bytes(0), "0 MB");
    }

    #[test]
    fn resources_line() {
        let gb = 1024 * 1024 * 1024;
        let text = resources_text(&Resources {
            server_memory: 6 * gb,
            server_cpu_percent: 23.4,
            pc_memory_used: 12 * gb,
            pc_memory_total: 16 * gb,
            started_unix: 1_700_000_000,
        });
        assert_eq!(
            text,
            "🧠 Server RAM: **6.0 GB** · ⚙️ CPU: **23%**\n\
             🖥️ PC RAM: 12.0 GB / 16.0 GB used (75%), 4.0 GB free\n\
             🕒 Started <t:1700000000:f> (<t:1700000000:R>)"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_secs(9)), "9s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 02m");
    }

    #[test]
    fn status_running_with_players() {
        let status = Status::Running {
            uptime: Duration::from_secs(3720),
            players: vec!["Mr_Bob".into(), "Steve".into()],
        };
        assert_eq!(
            format_status("gtnh", 25565, &status),
            "🟢 **gtnh** is online · port `25565` · up 1h 02m\nPlayers (2): Mr\\_Bob, Steve"
        );
    }

    #[test]
    fn status_running_empty_and_starting_and_offline() {
        let running = Status::Running { uptime: Duration::from_secs(5), players: vec![] };
        assert!(format_status("gtnh", 1, &running).ends_with("No players online"));

        let starting = Status::Starting { elapsed: Duration::from_secs(80) };
        assert_eq!(format_status("gtnh", 1, &starting), "🟡 **gtnh** is starting (1m 20s so far)");

        let clean_exit = Status::Stopped {
            last_exit: Some(LastExit { code: Some(0), crashed: false, recent_lines: vec!["bye".into()] }),
        };
        assert_eq!(format_status("gtnh", 1, &clean_exit), "🔴 **gtnh** is offline");
    }

    #[test]
    fn status_crash_shows_last_lines_safely() {
        let mut lines: Vec<String> = (1..=12).map(|i| format!("line {i}")).collect();
        lines.push("<Steve> ``` @everyone".into());
        let status = Status::Stopped {
            last_exit: Some(LastExit { code: Some(1), crashed: true, recent_lines: lines }),
        };
        let text = format_status("gtnh", 1, &status);

        assert!(text.contains("crashed last time (exit code 1)"), "{text}");
        assert!(!text.contains("line 5\n"), "only the last {LOG_LINES_SHOWN} lines: {text}");
        assert!(text.contains("line 12"), "{text}");
        // The player's ``` must not close the code block early.
        assert_eq!(text.matches("```").count(), 2, "{text}");
        assert!(text.chars().count() < 2000);
    }

    #[test]
    fn long_log_lines_are_cut() {
        let block = log_block(&["x".repeat(500)]);
        assert!(block.chars().count() < LOG_LINE_MAX_CHARS + 20, "{}", block.len());
    }

    #[test]
    fn markdown_is_escaped() {
        assert_eq!(escape_markdown("a_b*c`d"), "a\\_b\\*c\\`d");
        assert_eq!(escape_markdown("Steve"), "Steve");
    }
}
