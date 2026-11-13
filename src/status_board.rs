//! The live status channel (set with `/game action:setup`).
//!
//! The bot keeps one message per game in that channel and edits it when the
//! game's state changes (plus a refresh every couple of minutes for uptime).
//! When a server crashes, it posts a separate alert.
//!
//! The channel and message ids are saved to a JSON file, so after a bot restart
//! it edits the same messages instead of posting new ones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bot_discord::games::launcher::GameState;
use bot_discord::games::client::LauncherClient;
use bot_discord::games::manager::Status;
use serde::{Deserialize, Serialize};
use serenity::builder::{CreateAllowedMentions, CreateMessage, EditMessage};
use serenity::http::Http;
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::commands::game::{escape_markdown, format_status};

/// How often to ask the launcher for news.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// Re-edit even without changes this often, so uptime stays roughly current.
const HEARTBEAT: Duration = Duration::from_secs(120);
const STATE_FILE: &str = "status_board.json";

/// What's saved to disk.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Saved {
	channel_id: Option<u64>,
	/// game name -> id of its status message
	messages: HashMap<String, u64>,
}

/// What we remember about each game between polls (not saved).
#[derive(Debug, Default)]
struct Seen {
	/// Summary of the last text we posted. Unchanged = no edit needed.
	key: Option<String>,
	last_edit: Option<Instant>,
	/// Was it up (running or starting) at the last poll?
	was_up: bool,
}

struct Inner {
	saved: Saved,
	seen: HashMap<String, Seen>,
	/// The last refresh couldn't reach the launcher (to log only on changes).
	launcher_down: bool,
}

pub struct StatusBoard {
	path: PathBuf,
	inner: Mutex<Inner>,
	started: AtomicBool,
}

impl StatusBoard {
	/// Load saved state from `<data_dir>/status_board.json` (missing = fresh start).
	pub fn load(data_dir: &Path) -> Self {
		let path = data_dir.join(STATE_FILE);
		let saved = match std::fs::read_to_string(&path) {
			Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
				eprintln!("[status] ignoring unreadable {}: {error}", path.display());
				Saved::default()
			}),
			Err(_) => Saved::default(),
		};
		Self { path, inner: Mutex::new(Inner { saved, seen: HashMap::new(), launcher_down: false }), started: AtomicBool::new(false) }
	}

	/// Start the background loop. Safe to call on every `ready` (Discord
	/// reconnects fire it again): only the first call starts a loop.
	pub fn start(self: &Arc<Self>, http: Arc<Http>, launcher: Arc<LauncherClient>) {
		if self.started.swap(true, Ordering::SeqCst) {
			return;
		}
		let board = Arc::clone(self);
		tokio::spawn(async move {
			let mut interval = tokio::time::interval(POLL_INTERVAL);
			loop {
				interval.tick().await;
				if let Err(error) = board.refresh(&http, &launcher).await {
					eprintln!("[status] {error}");
				}
			}
		});
	}

	/// Make `channel` the status channel. Old messages elsewhere are left alone.
	/// Posts the messages right away, so a missing permission shows up now.
	pub async fn set_channel(&self, channel: ChannelId, http: &Http, launcher: &LauncherClient) -> Result<(), String> {
		{
			let mut inner = self.inner.lock().await;
			inner.saved = Saved { channel_id: Some(channel.get()), messages: HashMap::new() };
			inner.seen.clear();
			self.save(&inner.saved)?;
		}
		self.refresh(http, launcher).await
	}

	/// One round: ask the launcher, then post/edit what changed.
	async fn refresh(&self, http: &Http, launcher: &LauncherClient) -> Result<(), String> {
		// One refresh at a time (the loop and /game action:setup could overlap).
		let mut inner = self.inner.lock().await;
		let Some(channel) = inner.saved.channel_id.map(ChannelId::new) else {
			return Ok(()); // not set up yet
		};

		// Launcher unreachable: change nothing. The messages keep showing the last
		// known state, and updates resume by themselves once it's back.
		// Logged once when contact is lost and once when it's back (not every poll).
		let states = match fetch_states(launcher).await {
			Ok(states) => {
				if inner.launcher_down {
					inner.launcher_down = false;
					println!("[status] launcher reachable again, updating status messages");
				}
				states
			}
			Err(error) => {
				if !inner.launcher_down {
					inner.launcher_down = true;
					eprintln!("[status] can't reach the launcher, pausing updates: {error}");
				}
				return Ok(());
			}
		};

		let mut first_error = None;
		for state in &states {
			let name = state.info.name.clone();
			let text = board_text(state);
			let key = status_key(&state.status);

			// Crash alert: it was up last time, and now it's down with crashed = true.
			let seen = inner.seen.entry(name.clone()).or_default();
			if should_alert(seen.was_up, &state.status) {
				let alert = format!("⚠️ **{}** crashed!\n{}", escape_markdown(&name), format_status(&name, state.info.port, &state.status));
				if let Err(error) = send(http, channel, alert).await {
					first_error.get_or_insert(error);
				}
			}
			seen.was_up = !matches!(state.status, Status::Stopped { .. });

			let due = seen.last_edit.is_none_or(|at| at.elapsed() >= HEARTBEAT);
			if seen.key.as_deref() == Some(key.as_str()) && !due {
				continue;
			}

			let message_id = inner.saved.messages.get(&name).copied().map(MessageId::new);
			match upsert(http, channel, message_id, &text).await {
				Ok(id) => {
					let seen = inner.seen.entry(name.clone()).or_default();
					seen.key = Some(key);
					seen.last_edit = Some(Instant::now());
					if message_id != Some(id) {
						inner.saved.messages.insert(name, id.get());
						if let Err(error) = self.save(&inner.saved) {
							first_error.get_or_insert(error);
						}
					}
				}
				Err(error) => {
					first_error.get_or_insert(error);
				}
			}
		}

		match first_error {
			Some(error) => Err(error),
			None => Ok(()),
		}
	}

	/// Write via a temp file + rename, so a crash mid-write can't leave half a file.
	fn save(&self, saved: &Saved) -> Result<(), String> {
		if let Some(dir) = self.path.parent() {
			std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
		}
		let text = serde_json::to_string_pretty(saved).map_err(|e| e.to_string())?;
		let temp = self.path.with_extension("json.tmp");
		std::fs::write(&temp, text).map_err(|e| format!("could not write {}: {e}", temp.display()))?;
		std::fs::rename(&temp, &self.path).map_err(|e| format!("could not save {}: {e}", self.path.display()))
	}
}

async fn fetch_states(launcher: &LauncherClient) -> Result<Vec<GameState>, String> {
	let games = launcher.list().await.map_err(|error| error.to_string())?;
	let mut states = Vec::with_capacity(games.len());
	for game in games {
		states.push(launcher.state(&game.name).await.map_err(|error| error.to_string())?);
	}
	Ok(states)
}

/// Edit our message, or post a new one if we have none or it was deleted.
async fn upsert(http: &Http, channel: ChannelId, message_id: Option<MessageId>, text: &str) -> Result<MessageId, String> {
	if let Some(id) = message_id {
		let edit = EditMessage::new().content(text).allowed_mentions(CreateAllowedMentions::new());
		match channel.edit_message(http, id, edit).await {
			Ok(message) => return Ok(message.id),
			// Someone deleted it: fall through and post a fresh one.
			Err(serenity::Error::Http(error)) if error.status_code().is_some_and(|code| code.as_u16() == 404) => {}
			Err(error) => return Err(format!("could not edit status message: {error}")),
		}
	}
	send(http, channel, text.to_string()).await
}

async fn send(http: &Http, channel: ChannelId, text: String) -> Result<MessageId, String> {
	let message = CreateMessage::new().content(text).allowed_mentions(CreateAllowedMentions::new());
	channel
		.send_message(http, message)
		.await
		.map(|message| message.id)
		.map_err(|error| format!("could not post in the status channel (missing permission?): {error}"))
}

// ---------- pure helpers (tested below) ----------

fn board_text(state: &GameState) -> String {
	format!(
		"{}\n-# Updated <t:{}:R>",
		format_status(&state.info.name, state.info.port, &state.status),
		unix_now()
	)
}

/// What matters for "did it change?": the state and players, NOT the uptime
/// (that changes every second; the heartbeat takes care of it).
fn status_key(status: &Status) -> String {
	match status {
		Status::Running { players, .. } => format!("running:{}", players.join(",")),
		Status::Starting { .. } => "starting".to_string(),
		Status::Stopped { last_exit: Some(exit) } if exit.crashed => format!("crashed:{:?}", exit.code),
		Status::Stopped { .. } => "stopped".to_string(),
	}
}

/// Alert only on a transition we actually saw: up at the last poll, crashed now.
/// (So restarting the bot doesn't re-announce an old crash.)
fn should_alert(was_up: bool, now: &Status) -> bool {
	was_up && matches!(now, Status::Stopped { last_exit: Some(exit) } if exit.crashed)
}

fn unix_now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;
	use bot_discord::games::manager::LastExit;

	fn crashed(code: i32) -> Status {
		Status::Stopped { last_exit: Some(LastExit { code: Some(code), crashed: true, recent_lines: vec!["boom".into()] }) }
	}

	fn clean_stop() -> Status {
		Status::Stopped { last_exit: Some(LastExit { code: Some(0), crashed: false, recent_lines: vec![] }) }
	}

	fn running(players: &[&str], uptime_secs: u64) -> Status {
		Status::Running { uptime: Duration::from_secs(uptime_secs), players: players.iter().map(|p| p.to_string()).collect() }
	}

	#[test]
	fn key_ignores_uptime_but_not_players() {
		assert_eq!(status_key(&running(&["Steve"], 10)), status_key(&running(&["Steve"], 9999)));
		assert_ne!(status_key(&running(&["Steve"], 10)), status_key(&running(&["Steve", "Alex"], 10)));
		assert_ne!(status_key(&running(&[], 10)), status_key(&Status::Starting { elapsed: Duration::ZERO }));
		assert_ne!(status_key(&clean_stop()), status_key(&crashed(1)));
	}

	#[test]
	fn alerts_only_on_a_seen_crash() {
		assert!(should_alert(true, &crashed(1)));
		// Bot just started and the last run had crashed long ago: no alert.
		assert!(!should_alert(false, &crashed(1)));
		// A normal /game stop is not a crash.
		assert!(!should_alert(true, &clean_stop()));
		assert!(!should_alert(true, &running(&[], 1)));
	}

	#[test]
	fn saved_state_round_trips_and_survives_bad_files() {
		let dir = std::env::temp_dir().join(format!("bot_discord_board_{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);

		// Missing file = fresh start.
		let board = StatusBoard::load(&dir);
		assert_eq!(board.inner.try_lock().unwrap().saved, Saved::default());

		let saved = Saved { channel_id: Some(123), messages: HashMap::from([("gtnh".to_string(), 456)]) };
		board.save(&saved).unwrap();
		assert_eq!(StatusBoard::load(&dir).inner.try_lock().unwrap().saved, saved);

		// A corrupt file is ignored, not a crash.
		std::fs::write(dir.join(STATE_FILE), "{ not json").unwrap();
		assert_eq!(StatusBoard::load(&dir).inner.try_lock().unwrap().saved, Saved::default());
	}

	#[test]
	fn board_text_has_a_live_timestamp() {
		let state = GameState {
			info: bot_discord::games::launcher::GameInfo { name: "gtnh".into(), port: 25565, stop_timeout_secs: 180 },
			status: running(&["Steve"], 60),
			resources: None,
			ping: None,
		};
		let text = board_text(&state);
		assert!(text.starts_with("🟢 **gtnh** is online"), "{text}");
		assert!(text.contains("-# Updated <t:") && text.ends_with(":R>"), "{text}");
	}
}
