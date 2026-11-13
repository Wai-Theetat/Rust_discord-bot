//! Keeps track of every game server: starts processes, reads their output,
//! and notices when they exit (cleanly or by crashing).
//!
//! Ownership layout for one running server:
//!
//!   Game (shared via Arc) ── state: Mutex<State>
//!       └─ State::Running holds: stdin (to send commands), kill switch, players, ...
//!
//!   3 background tasks per run, each holding an Arc<Game>:
//!       - stdout reader ─┐ parse lines -> update state
//!       - stderr reader ─┘
//!       - exit watcher: OWNS the Child, waits for it to exit, then marks the game Stopped
//!
//! The Child can't live inside the Mutex: `child.wait()` needs it borrowed for as
//! long as the server runs, which would keep the lock held the whole time.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;

use super::config::{Config, GameConfig, GameKind};
use super::events::{Event, parse_line};

/// How many recent console lines to keep (shown when a server crashes).
const RECENT_LINES: usize = 20;
/// After the process exits, wait at most this long for the readers to drain.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// After a force-kill, wait at most this long for the process to be gone.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Max time for one TShock REST request.
const REST_TIMEOUT: Duration = Duration::from_secs(10);
/// Max time to ask TShock who's online (part of every status reply).
const PLAYER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_TSHOCK_REST_PORT: u16 = 7878;
/// Environment variable holding the TShock REST token (from .env).
const TSHOCK_TOKEN_VAR: &str = "TSHOCK_REST_TOKEN";

/// Every run gets a unique id, so tasks from an old run can't touch a newer one.
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

// ---------- public types ----------

/// All configured games. The set of games is fixed at startup.
pub struct GameManager {
	games: HashMap<String, Arc<Game>>,
}

impl GameManager {
	pub fn new(config: Config) -> Self {
		let games = config
			.games
			.into_iter()
			.map(|(name, config)| (name.clone(), Arc::new(Game::new(name, config))))
			.collect();
		Self { games }
	}

	pub fn get(&self, name: &str) -> Option<Arc<Game>> {
		self.games.get(name).cloned()
	}

	/// Every game, in no particular order.
	pub fn all(&self) -> impl Iterator<Item = &Arc<Game>> {
		self.games.values()
	}

	/// Game names in alphabetical order (for autocomplete and listings).
	pub fn names(&self) -> Vec<&str> {
		let mut names: Vec<&str> = self.games.keys().map(String::as_str).collect();
		names.sort_unstable();
		names
	}
}

/// What `/game check` shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Status {
	Stopped { last_exit: Option<LastExit> },
	/// Process is running but hasn't printed its "ready" line yet.
	Starting { elapsed: Duration },
	Running { uptime: Duration, players: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastExit {
	/// Exit code, if the OS reported one.
	pub code: Option<i32>,
	/// True if it exited without anyone asking it to stop.
	pub crashed: bool,
	/// The last console lines before it exited.
	pub recent_lines: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum StartOutcome {
	Started,
	/// It was already running; here's its status instead.
	AlreadyRunning(Status),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum StopOutcome {
	NotRunning,
	/// It saved and exited by itself after the stop request.
	Stopped,
	/// It didn't exit within `stop_timeout_secs`, so it was killed (no save).
	ForceKilled,
}

// ---------- one game ----------

pub struct Game {
	pub name: String,
	pub config: GameConfig,
	state: Mutex<State>,
	/// The run_id of the most recent run that exited. `stop()` subscribes to it
	/// to wait for the exit without polling.
	exits: watch::Sender<u64>,
}

enum State {
	Stopped { last_exit: Option<LastExit> },
	Running(Running),
}

struct Running {
	run_id: u64,
	stdin: ChildStdin,
	/// Send () to force-kill. Taken (None) once used.
	kill: Option<oneshot::Sender<()>>,
	started_at: Instant,
	/// Wall-clock start time, for "started at 14:03".
	started_unix: u64,
	/// OS process id, for measuring RAM/CPU. None if the OS didn't give one.
	pid: Option<u32>,
	ready_at: Option<Instant>,
	players: BTreeSet<String>,
	/// Set before a deliberate stop/kill, so the exit isn't reported as a crash.
	stop_requested: bool,
	recent: VecDeque<String>,
}

impl Game {
	fn new(name: String, config: GameConfig) -> Self {
		Self {
			name,
			config,
			state: Mutex::new(State::Stopped { last_exit: None }),
			exits: watch::channel(0).0,
		}
	}

	/// Start the server. If it's already running, return its status instead.
	///
	/// Takes `self: &Arc<Self>` because the background tasks need their own Arc.
	pub async fn start(self: &Arc<Self>) -> Result<StartOutcome, String> {
		// Holding the lock for the whole start means two `/game start`s at the
		// same moment can't both launch a process: the second one waits here,
		// then sees Running.
		let mut state = self.state.lock().await;
		if matches!(*state, State::Running(_)) {
			return Ok(StartOutcome::AlreadyRunning(snapshot(&state)));
		}

		let (program, args) = self
			.config
			.start
			.split_first()
			.ok_or_else(|| format!("`start` for {} is empty", self.name))?;
		let program = resolve_program(&self.config.dir, program);

		let mut child = Command::new(&program)
			.args(args)
			.current_dir(&self.config.dir)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			// If the exit watcher (which owns the Child) is ever dropped without
			// the process exiting, kill it rather than leave an unmanaged server.
			// The launcher always runs stop_all first, so this never skips a save there;
			// it mainly stops failing tests from leaving processes behind.
			.kill_on_drop(true)
			.spawn()
			.map_err(|error| format!("Could not start {}: {error}", program.display()))?;

		let stdin = child.stdin.take().expect("stdin is piped");
		let stdout = child.stdout.take().expect("stdout is piped");
		let stderr = child.stderr.take().expect("stderr is piped");

		let run_id = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
		let (kill_tx, kill_rx) = oneshot::channel();
		let pid = child.id();
		let started_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

		*state = State::Running(Running {
			run_id,
			stdin,
			kill: Some(kill_tx),
			started_at: Instant::now(),
			started_unix,
			pid,
			ready_at: None,
			players: BTreeSet::new(),
			stop_requested: false,
			recent: VecDeque::with_capacity(RECENT_LINES),
		});
		// Release the lock before the tasks start, since they lock it too.
		drop(state);

		let readers = [
			tokio::spawn(read_output(Arc::clone(self), run_id, stdout)),
			tokio::spawn(read_output(Arc::clone(self), run_id, stderr)),
		];
		tokio::spawn(watch_exit(Arc::clone(self), run_id, child, kill_rx, readers));

		Ok(StartOutcome::Started)
	}

	pub async fn status(&self) -> Status {
		snapshot(&*self.state.lock().await)
	}

	/// Who's online, asked from the server itself instead of read from the log.
	/// Terraria: TShock's REST API (its log format for joins isn't reliable).
	/// Minecraft: None, the console's join/leave lines are already exact.
	/// Also None if the server isn't ready or TShock can't be asked.
	pub async fn live_players(&self) -> Option<Vec<String>> {
		if self.config.kind != GameKind::Terraria || !matches!(self.status().await, Status::Running { .. }) {
			return None;
		}
		let token = std::env::var(TSHOCK_TOKEN_VAR).unwrap_or_default();
		let port = self.config.rest_port.unwrap_or(DEFAULT_TSHOCK_REST_PORT);
		// Short limit: a slow TShock must never hold up status replies. On a
		// timeout, the console-based player list is used instead.
		match tokio::time::timeout(PLAYER_LOOKUP_TIMEOUT, tshock_players(port, &token)).await {
			Ok(Ok(players)) => Some(players),
			_ => None,
		}
	}

	/// The running process, if any: (OS process id, start time as Unix seconds).
	pub async fn process(&self) -> Option<(u32, u64)> {
		match &*self.state.lock().await {
			State::Running(running) => running.pid.map(|pid| (pid, running.started_unix)),
			State::Stopped { .. } => None,
		}
	}

	/// Type one line into the server console.
	pub async fn send_command(&self, command: &str) -> Result<(), String> {
		let mut state = self.state.lock().await;
		let State::Running(running) = &mut *state else {
			return Err(format!("{} is not running", self.name));
		};
		let line = format!("{command}\n");
		running
			.stdin
			.write_all(line.as_bytes())
			.await
			.map_err(|error| format!("Could not send to {}: {error}", self.name))?;
		running.stdin.flush().await.map_err(|error| error.to_string())
	}

	/// Stop cleanly: ask the server to save and quit, wait up to
	/// `stop_timeout_secs`, and force-kill only if it doesn't exit in time.
	pub async fn stop(&self) -> Result<StopOutcome, String> {
		let (run_id, mut exits) = {
			let mut state = self.state.lock().await;
			let State::Running(running) = &mut *state else {
				return Ok(StopOutcome::NotRunning);
			};

			// Subscribe while holding the lock: the exit watcher needs this lock
			// to report the exit, so the exit can't slip past before we listen.
			let exits = self.exits.subscribe();

			// A second `/game stop` while one is in progress just waits too.
			if !running.stop_requested {
				running.stop_requested = true;
				if let Err(error) = request_stop(&self.config, running).await {
					// The request never reached the server, so it's not stopping.
					running.stop_requested = false;
					return Err(error);
				}
			}
			(running.run_id, exits)
		};

		let timeout = Duration::from_secs(self.config.stop_timeout_secs);
		if wait_for_exit(&mut exits, run_id, timeout).await {
			return Ok(StopOutcome::Stopped);
		}

		self.kill().await;
		if !wait_for_exit(&mut exits, run_id, KILL_TIMEOUT).await {
			return Err(format!("{} did not exit even after being killed", self.name));
		}
		Ok(StopOutcome::ForceKilled)
	}

	/// Stop (if running), then start again.
	pub async fn restart(self: &Arc<Self>) -> Result<StartOutcome, String> {
		self.stop().await?;
		self.start().await
	}

	/// Force-kill right away (no save). Returns false if it wasn't running.
	pub async fn kill(&self) -> bool {
		let mut state = self.state.lock().await;
		let State::Running(running) = &mut *state else {
			return false;
		};
		running.stop_requested = true;
		if let Some(kill) = running.kill.take() {
			let _ = kill.send(());
		}
		true
	}

	/// Called by the readers for every console line.
	async fn on_line(&self, run_id: u64, line: String) {
		let mut state = self.state.lock().await;
		let State::Running(running) = &mut *state else { return };
		if running.run_id != run_id {
			return; // a line from an older run
		}

		match parse_line(self.config.kind, &line) {
			Some(Event::Ready) => {
				running.ready_at.get_or_insert_with(Instant::now);
			}
			Some(Event::Joined(name)) => {
				running.players.insert(name);
			}
			Some(Event::Left(name)) => {
				running.players.remove(&name);
			}
			None => {}
		}

		if running.recent.len() == RECENT_LINES {
			running.recent.pop_front();
		}
		running.recent.push_back(line);
	}

	/// Called once by the exit watcher when the process is gone.
	async fn on_exit(&self, run_id: u64, status: std::io::Result<ExitStatus>) {
		let mut state = self.state.lock().await;
		let State::Running(running) = &mut *state else { return };
		if running.run_id != run_id {
			return;
		}

		let last_exit = LastExit {
			code: status.ok().and_then(|status| status.code()),
			crashed: !running.stop_requested,
			recent_lines: running.recent.iter().cloned().collect(),
		};
		*state = State::Stopped { last_exit: Some(last_exit) };
		// Wake anyone waiting in `stop()`. send_replace works even with no listeners.
		self.exits.send_replace(run_id);
	}
}

// ---------- stopping ----------

/// Send the game's "save and quit" request. Called with the state lock held.
async fn request_stop(config: &GameConfig, running: &mut Running) -> Result<(), String> {
	match config.kind {
		// Minecraft reads its console: type `stop`.
		GameKind::Minecraft => {
			running.stdin.write_all(b"stop\n").await.map_err(|error| error.to_string())?;
			running.stdin.flush().await.map_err(|error| error.to_string())
		}
		// Terraria ignores piped input, so go through TShock's REST API.
		GameKind::Terraria => {
			let token = std::env::var(TSHOCK_TOKEN_VAR).unwrap_or_default();
			let port = config.rest_port.unwrap_or(DEFAULT_TSHOCK_REST_PORT);
			tshock_off(port, &token).await
		}
	}
}

/// Wait until the run `run_id` has exited. False on timeout.
async fn wait_for_exit(exits: &mut watch::Receiver<u64>, run_id: u64, timeout: Duration) -> bool {
	// run_ids only grow, so ">=" also covers "a newer run already exited too".
	let exited = exits.wait_for(|last_exited| *last_exited >= run_id);
	matches!(tokio::time::timeout(timeout, exited).await, Ok(Ok(_)))
}

/// Ask TShock who's online.
async fn tshock_players(port: u16, token: &str) -> Result<Vec<String>, String> {
	check_tshock_token(token)?;
	let reply = http_get(port, &format!("/v2/server/status?token={token}&players=true")).await?;
	let body = reply.text.split("\r\n\r\n").nth(1).unwrap_or("");
	parse_tshock_players(body).ok_or_else(|| "TShock sent an unexpected status reply".to_string())
}

/// `{"status": "200", "players": [...]}`. Each player is an object with a
/// "nickname" (or a plain string in some TShock versions).
fn parse_tshock_players(body: &str) -> Option<Vec<String>> {
	let json: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
	if json["status"] != "200" {
		return None;
	}
	// Some TShock versions send one string: "Steve, Alex" ("" when empty).
	if let Some(list) = json["players"].as_str() {
		let names = list.split(", ").map(str::trim).filter(|name| !name.is_empty());
		return Some(names.map(|name| name.chars().take(32).collect()).collect());
	}
	let players = json["players"].as_array()?;
	let names = players
		.iter()
		.filter_map(|player| match player {
			serde_json::Value::String(name) => Some(name.clone()),
			other => other["nickname"].as_str().or_else(|| other["name"].as_str()).map(str::to_string),
		})
		// Names are shown in Discord; keep them a sane length.
		.map(|name| name.chars().take(32).collect())
		.collect();
	Some(names)
}

/// The token goes into a URL, so only allow characters that need no escaping.
/// (Also stops a token with spaces or '&' from changing the request.)
fn check_tshock_token(token: &str) -> Result<(), String> {
	if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
		return Err(format!(
			"{TSHOCK_TOKEN_VAR} is missing, or has characters other than letters, digits, '-' and '_'"
		));
	}
	Ok(())
}

/// Ask TShock to save the world and shut down.
async fn tshock_off(port: u16, token: &str) -> Result<(), String> {
	check_tshock_token(token)?;
	let path = format!(
		"/v2/server/off?token={token}&confirm=true&nosave=false&message=Server+stopped+from+Discord"
	);
	let reply = http_get(port, &path).await?;

	// TShock answers with JSON like {"status": "200", "response": "..."}.
	// Remove whitespace so formatting differences don't matter.
	let compact: String = reply.text.chars().filter(|c| !c.is_whitespace()).collect();
	if compact.contains(r#""status":"200""#) {
		return Ok(());
	}
	// TShock often shuts down so fast that it kills its own web server before
	// the answer is sent ("connection forcibly closed"). The request was
	// delivered, so the shutdown is underway. This is not a blind "OK": stop()
	// still waits for the process to exit and force-kills if it doesn't.
	if reply.cut_off && !compact.contains(r#""status":""#) {
		return Ok(());
	}
	// Never include the request path in errors: it contains the token.
	let body = reply.text.split("\r\n\r\n").nth(1).unwrap_or("").trim();
	Err(format!("TShock refused the stop request: {}", truncate(body, 200)))
}

struct HttpReply {
	/// Whatever arrived (headers + body), possibly nothing.
	text: String,
	/// The server hung up before finishing the answer.
	cut_off: bool,
}

/// A minimal HTTP/1.1 GET to 127.0.0.1.
///
/// Failing to connect or to send is an error. Once the request is sent, the
/// server hanging up early is reported as `cut_off`, not an error, because
/// for a shutdown request that's the expected outcome.
async fn http_get(port: u16, path: &str) -> Result<HttpReply, String> {
	let request = async {
		let mut stream = TcpStream::connect(("127.0.0.1", port))
			.await
			.map_err(|error| format!("Could not reach TShock REST on port {port}: {error}"))?;
		let request =
			format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
		stream
			.write_all(request.as_bytes())
			.await
			.map_err(|error| format!("Could not send to TShock REST on port {port}: {error}"))?;

		// Read until the answer is complete. TShock ignores "Connection: close"
		// and keeps the socket open, so waiting for the close would hang until
		// the timeout. It does send Content-Length, so stop once that's in.
		let mut response = Vec::new();
		let mut chunk = [0u8; 4096];
		let mut cut_off = false;
		while !response_complete(&response) {
			match stream.read(&mut chunk).await {
				Ok(0) => break,
				Ok(n) => response.extend_from_slice(&chunk[..n]),
				Err(_) => {
					cut_off = true;
					break;
				}
			}
		}
		// A clean close with no answer at all also counts as cut off.
		cut_off |= response.is_empty();
		Ok(HttpReply { text: String::from_utf8_lossy(&response).into_owned(), cut_off })
	};
	match tokio::time::timeout(REST_TIMEOUT, request).await {
		Ok(result) => result,
		Err(_) => Err(format!("TShock REST on port {port} did not answer in time")),
	}
}

/// True once the headers and the whole body (per Content-Length) have arrived.
/// Without a Content-Length we can only wait for the server to close.
fn response_complete(response: &[u8]) -> bool {
	let Some(header_end) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
		return false;
	};
	let headers = String::from_utf8_lossy(&response[..header_end]);
	let content_length = headers.lines().find_map(|line| {
		let (name, value) = line.split_once(':')?;
		name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
	});
	match content_length {
		Some(length) => response.len() - (header_end + 4) >= length,
		None => false,
	}
}

fn truncate(text: &str, max_chars: usize) -> String {
	text.chars().take(max_chars).collect()
}

// ---------- background tasks ----------

/// Read a stdout/stderr stream line by line and feed it to the game.
async fn read_output<R>(game: Arc<Game>, run_id: u64, stream: R)
where
	R: AsyncRead + Unpin,
{
	let mut reader = BufReader::new(stream);
	let mut buf = Vec::new();
	loop {
		buf.clear();
		match reader.read_until(b'\n', &mut buf).await {
			Ok(0) | Err(_) => break,
			Ok(_) => {
				let text = String::from_utf8_lossy(&buf);
				// Progress bars redraw with '\r' ("Settling liquids 42%"): keep the last one.
				let line = text.trim_end().rsplit('\r').next().unwrap_or("").trim();
				if !line.is_empty() {
					game.on_line(run_id, line.to_string()).await;
				}
			}
		}
	}
}

/// Owns the Child. Waits for it to exit (or for the kill switch), then reports.
async fn watch_exit(
	game: Arc<Game>,
	run_id: u64,
	mut child: Child,
	mut kill_rx: oneshot::Receiver<()>,
	readers: [JoinHandle<()>; 2],
) {
	// `Ok(()) = &mut kill_rx` only matches a real kill. If the sender is dropped
	// instead, that branch is disabled and we keep waiting for a normal exit.
	let exited = tokio::select! {
		status = child.wait() => Some(status),
		Ok(()) = &mut kill_rx => None,
	};
	let status = match exited {
		Some(status) => status,
		None => {
			let _ = child.start_kill();
			child.wait().await
		}
	};

	// Let the readers catch the last lines (often the crash reason), but don't
	// hang if some leftover child process keeps the pipe open.
	for reader in readers {
		let _ = tokio::time::timeout(DRAIN_TIMEOUT, reader).await;
	}

	game.on_exit(run_id, status).await;
}

// ---------- helpers ----------

fn snapshot(state: &State) -> Status {
	match state {
		State::Stopped { last_exit } => Status::Stopped { last_exit: last_exit.clone() },
		State::Running(running) => match running.ready_at {
			None => Status::Starting { elapsed: running.started_at.elapsed() },
			Some(ready_at) => Status::Running {
				uptime: ready_at.elapsed(),
				players: running.players.iter().cloned().collect(),
			},
		},
	}
}

/// Windows doesn't look for a bare program name inside `current_dir`, so if the
/// program exists in the server folder, use that full path. Otherwise keep the
/// name as-is (e.g. "java", found on PATH).
fn resolve_program(dir: &str, program: &str) -> PathBuf {
	let path = Path::new(program);
	if path.is_relative() {
		let in_dir = Path::new(dir).join(path);
		if in_dir.is_file() {
			return in_dir;
		}
	}
	path.to_path_buf()
}

// ---------- tests (fake servers made of .bat files) ----------

#[cfg(all(test, windows))]
mod tests {
	use super::*;
	use crate::games::config::GameKind;

	/// A throwaway folder with one fake "server" script in it.
	fn fake_server(test_name: &str, script: &str) -> Arc<Game> {
		fake_server_with_timeout(test_name, script, 5)
	}

	fn fake_server_with_timeout(test_name: &str, script: &str, stop_timeout_secs: u64) -> Arc<Game> {
		let dir = std::env::temp_dir().join(format!("bot_discord_test_{test_name}_{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("server.bat"), script.replace('\n', "\r\n")).unwrap();

		let config = GameConfig {
			kind: GameKind::Minecraft,
			dir: dir.to_string_lossy().into_owned(),
			// `.\` because cmd may be told not to search the current folder
			// (NoDefaultCurrentDirectoryInExePath), same idea as resolve_program.
			start: vec!["cmd".into(), "/c".into(), r".\server.bat".into()],
			port: 1,
			stop_timeout_secs,
			rest_port: None,
		};
		Arc::new(Game::new(test_name.to_string(), config))
	}

	/// Poll the status until `check` is true, or fail after 10 seconds.
	async fn wait_for(game: &Game, what: &str, check: impl Fn(&Status) -> bool) -> Status {
		for _ in 0..200 {
			let status = game.status().await;
			if check(&status) {
				return status;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		panic!("timed out waiting for {what}; last status: {:?}", game.status().await);
	}

	impl Game {
		/// Test helper: the newest console line of the current run.
		async fn last_line(&self) -> Option<String> {
			match &*self.state.lock().await {
				State::Running(running) => running.recent.back().cloned(),
				State::Stopped { .. } => None,
			}
		}
	}

	const DONE: &str =r#"echo [00:00:00] [Server thread/INFO]: Done (1.0s)! For help, type "help""#;

	#[tokio::test]
	async fn tracks_ready_and_players_then_kill_is_not_a_crash() {
		let script = format!(
			"@echo off\n{DONE}\n\
			 echo [00:00:01] [Server thread/INFO]: Steve joined the game\n\
			 echo [00:00:02] [Server thread/INFO]: Alex joined the game\n\
			 echo [00:00:03] [Server thread/INFO]: Alex left the game\n\
			 set /p wait=\n"
		);
		let game = fake_server("players", &script);

		assert_eq!(game.status().await, Status::Stopped { last_exit: None });
		assert_eq!(game.start().await.unwrap(), StartOutcome::Started);

		let status = wait_for(&game, "Steve only", |s| {
			matches!(s, Status::Running { players, .. } if players == &["Steve"])
		})
		.await;
		assert!(matches!(status, Status::Running { .. }));

		// Starting again doesn't launch a second process.
		assert!(matches!(game.start().await.unwrap(), StartOutcome::AlreadyRunning(Status::Running { .. })));

		assert!(game.kill().await);
		let status = wait_for(&game, "stopped", |s| matches!(s, Status::Stopped { .. })).await;
		let Status::Stopped { last_exit: Some(exit) } = status else { panic!() };
		assert!(!exit.crashed, "a requested kill is not a crash");
	}

	#[tokio::test]
	async fn join_then_leave_leaves_no_players() {
		let script = format!(
			"@echo off\n{DONE}\n\
			 echo [00:00:01] [Server thread/INFO]: Steve joined the game\n\
			 echo [00:00:02] [Server thread/INFO]: Steve left the game\n\
			 echo [00:00:03] [Server thread/INFO]: all done\n\
			 set /p wait=\n"
		);
		let game = fake_server("join_leave", &script);
		game.start().await.unwrap();

		// Careful: "players is empty" is ALSO true right after Done, before Steve
		// joins. So first wait for the last line ("all done"), which comes after
		// the leave. Then the empty list really means "joined, then left".
		for _ in 0..200 {
			if game.last_line().await.is_some_and(|line| line.contains("all done")) {
				break;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}

		let status = game.status().await;
		assert!(
			matches!(&status, Status::Running { players, .. } if players.is_empty()),
			"{status:?}"
		);

		game.kill().await;
	}

	#[tokio::test]
	async fn exiting_on_its_own_is_a_crash_with_last_lines() {
		let script = format!("@echo off\n{DONE}\necho java.lang.OutOfMemoryError: oops\nexit /b 3\n");
		let game = fake_server("crash", &script);
		game.start().await.unwrap();

		let status = wait_for(&game, "stopped", |s| matches!(s, Status::Stopped { last_exit: Some(_) })).await;
		let Status::Stopped { last_exit: Some(exit) } = status else { panic!() };
		assert!(exit.crashed);
		assert_eq!(exit.code, Some(3));
		assert!(exit.recent_lines.iter().any(|l| l.contains("OutOfMemoryError")), "{:?}", exit.recent_lines);
	}

	#[tokio::test]
	async fn send_command_reaches_the_server() {
		let script = format!("@echo off\n{DONE}\nset /p cmd=\necho got %cmd%\n");
		let game = fake_server("stdin", &script);
		game.start().await.unwrap();
		wait_for(&game, "ready", |s| matches!(s, Status::Running { .. })).await;

		game.send_command("hello").await.unwrap();

		let status = wait_for(&game, "stopped", |s| matches!(s, Status::Stopped { last_exit: Some(_) })).await;
		let Status::Stopped { last_exit: Some(exit) } = status else { panic!() };
		assert!(exit.recent_lines.iter().any(|l| l == "got hello"), "{:?}", exit.recent_lines);
	}

	#[tokio::test]
	async fn not_running_commands_fail_cleanly() {
		let game = fake_server("idle", "@echo off\n");
		assert!(game.send_command("stop").await.is_err());
		assert!(!game.kill().await);
	}

	#[tokio::test]
	async fn missing_program_is_an_error() {
		let game = fake_server("missing", "@echo off\n");
		let mut config = game.config.clone();
		config.start = vec!["definitely-not-a-real-program-xyz".into()];
		let game = Game::new("missing".into(), config);
		let error = Arc::new(game).start().await.unwrap_err();
		assert!(error.contains("Could not start"), "{error}");
	}

	#[test]
	fn resolve_program_prefers_the_server_folder() {
		let dir = std::env::temp_dir().join(format!("bot_discord_resolve_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("Server.exe"), b"").unwrap();
		let dir_str = dir.to_string_lossy();

		assert_eq!(resolve_program(&dir_str, "Server.exe"), dir.join("Server.exe"));
		assert_eq!(resolve_program(&dir_str, "java"), PathBuf::from("java"));
	}

	// ----- Milestone 5: stop / restart -----

	/// Behaves like Minecraft: waits for input, quits on `stop`, ignores the rest.
	fn obeys_stop() -> String {
		format!(
			"@echo off\n{DONE}\n\
			 :loop\n\
			 set /p cmd=\n\
			 if \"%cmd%\"==\"stop\" goto stop\n\
			 goto loop\n\
			 :stop\n\
			 echo [00:00:09] [Server thread/INFO]: Saving worlds\n\
			 exit /b 0\n"
		)
	}

	async fn expect_stopped_cleanly(game: &Game) {
		let Status::Stopped { last_exit: Some(exit) } = game.status().await else {
			panic!("not stopped: {:?}", game.status().await);
		};
		assert!(!exit.crashed, "a requested stop is not a crash");
	}

	#[tokio::test]
	async fn stop_asks_nicely_and_waits() {
		let game = fake_server("stop_ok", &obeys_stop());
		game.start().await.unwrap();
		wait_for(&game, "ready", |s| matches!(s, Status::Running { .. })).await;

		assert_eq!(game.stop().await.unwrap(), StopOutcome::Stopped);
		expect_stopped_cleanly(&game).await;

		// The server got to save: its last line made it into the log.
		let Status::Stopped { last_exit: Some(exit) } = game.status().await else { panic!() };
		assert!(exit.recent_lines.iter().any(|l| l.contains("Saving worlds")), "{:?}", exit.recent_lines);
	}

	#[tokio::test]
	async fn stop_force_kills_after_the_timeout() {
		// Ignores every command, like a hung server.
		let script = format!("@echo off\n{DONE}\n:loop\nset /p cmd=\ngoto loop\n");
		let game = fake_server_with_timeout("stop_stuck", &script, 1);
		game.start().await.unwrap();
		wait_for(&game, "ready", |s| matches!(s, Status::Running { .. })).await;

		let started = Instant::now();
		assert_eq!(game.stop().await.unwrap(), StopOutcome::ForceKilled);
		assert!(started.elapsed() >= Duration::from_secs(1), "must wait the full timeout first");
		expect_stopped_cleanly(&game).await;
	}

	#[tokio::test]
	async fn stop_when_not_running() {
		let game = fake_server("stop_idle", "@echo off\n");
		assert_eq!(game.stop().await.unwrap(), StopOutcome::NotRunning);
	}

	#[tokio::test]
	async fn two_stops_at_once_both_succeed() {
		let game = fake_server("stop_twice", &obeys_stop());
		game.start().await.unwrap();
		wait_for(&game, "ready", |s| matches!(s, Status::Running { .. })).await;

		// The second one must not send another `stop`, just wait for the same exit.
		let (first, second) = tokio::join!(game.stop(), game.stop());
		assert_eq!(first.unwrap(), StopOutcome::Stopped);
		assert!(matches!(second.unwrap(), StopOutcome::Stopped | StopOutcome::NotRunning));
		expect_stopped_cleanly(&game).await;
	}

	#[tokio::test]
	async fn restart_runs_it_again() {
		let game = fake_server("restart", &obeys_stop());
		game.start().await.unwrap();
		wait_for(&game, "ready", |s| matches!(s, Status::Running { .. })).await;

		assert_eq!(game.restart().await.unwrap(), StartOutcome::Started);
		wait_for(&game, "ready again", |s| matches!(s, Status::Running { .. })).await;

		assert_eq!(game.stop().await.unwrap(), StopOutcome::Stopped);
	}

	#[tokio::test]
	async fn restart_when_stopped_just_starts() {
		let game = fake_server("restart_idle", &obeys_stop());
		assert_eq!(game.restart().await.unwrap(), StartOutcome::Started);
		wait_for(&game, "ready", |s| matches!(s, Status::Running { .. })).await;
		game.stop().await.unwrap();
	}

	// ----- TShock REST (against a fake HTTP server) -----

	/// A one-request HTTP server on a random free port. Returns the port and a
	/// handle that resolves to the raw request it received.
	async fn fake_tshock(body: &'static str) -> (u16, JoinHandle<String>) {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		let handle = tokio::spawn(async move {
			let (mut socket, _) = listener.accept().await.unwrap();
			let mut buf = vec![0; 4096];
			let n = socket.read(&mut buf).await.unwrap();
			let response = format!(
				"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
				body.len()
			);
			socket.write_all(response.as_bytes()).await.unwrap();
			String::from_utf8_lossy(&buf[..n]).into_owned()
		});
		(port, handle)
	}

	#[tokio::test]
	async fn tshock_off_sends_the_right_request() {
		let (port, server) =
			fake_tshock("{\n  \"status\": \"200\",\n  \"response\": \"The server is shutting down\"\n}").await;

		tshock_off(port, "abc123").await.unwrap();

		let request = server.await.unwrap();
		let first_line = request.lines().next().unwrap();
		assert!(first_line.starts_with("GET /v2/server/off?token=abc123&confirm=true&nosave=false"), "{first_line}");
	}

	#[tokio::test]
	async fn tshock_off_reports_a_refusal_without_leaking_the_token() {
		let (port, _server) = fake_tshock(r#"{"status": "403", "error": "Not authorized."}"#).await;

		let error = tshock_off(port, "secret-token").await.unwrap_err();
		assert!(error.contains("Not authorized"), "{error}");
		assert!(!error.contains("secret-token"), "token leaked: {error}");
	}

	#[tokio::test]
	async fn tshock_off_when_nothing_is_listening() {
		// Grab a free port, then close it so nobody listens there.
		let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
		let error = tshock_off(port, "secret-token").await.unwrap_err();
		assert!(error.contains("Could not reach"), "{error}");
		assert!(!error.contains("secret-token"), "token leaked: {error}");
	}

	/// Like real TShock during shutdown: reads the request, then hangs up
	/// without answering. `reset` = slam it shut (RST, "forcibly closed"),
	/// otherwise a normal close with no data.
	async fn fake_tshock_that_hangs_up(reset: bool) -> (u16, JoinHandle<String>) {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		let handle = tokio::spawn(async move {
			let (mut socket, _) = listener.accept().await.unwrap();
			let mut buf = vec![0; 4096];
			let n = socket.read(&mut buf).await.unwrap();
			if reset {
				// Linger 0 makes the close send a reset instead of a normal goodbye.
				#[allow(deprecated)]
				socket.set_linger(Some(Duration::ZERO)).unwrap();
			}
			drop(socket);
			String::from_utf8_lossy(&buf[..n]).into_owned()
		});
		(port, handle)
	}

	#[tokio::test]
	async fn tshock_off_accepts_a_reset_during_shutdown() {
		// Exactly what the real server did: "An existing connection was forcibly closed".
		let (port, server) = fake_tshock_that_hangs_up(true).await;
		tshock_off(port, "abc123").await.expect("a reset after sending means TShock is shutting down");
		assert!(server.await.unwrap().contains("/v2/server/off"));
	}

	#[tokio::test]
	async fn tshock_off_accepts_a_silent_close_during_shutdown() {
		let (port, _server) = fake_tshock_that_hangs_up(false).await;
		tshock_off(port, "abc123").await.expect("closing without an answer means TShock is shutting down");
	}

	#[test]
	fn parses_tshock_player_lists() {
		// Exactly what your TShock answered during the REST test (nobody on).
		let empty = r#"{"status": "200", "name": "", "port": 7777, "playercount": 0, "maxplayers": 8, "players": []}"#;
		assert_eq!(parse_tshock_players(empty), Some(vec![]));

		let two = r#"{"status": "200", "players": [{"nickname": "Steve", "username": ""}, {"nickname": "Big Alex"}]}"#;
		assert_eq!(parse_tshock_players(two), Some(vec!["Steve".to_string(), "Big Alex".to_string()]));

		// Some TShock versions send plain names.
		assert_eq!(parse_tshock_players(r#"{"status": "200", "players": ["Steve"]}"#), Some(vec!["Steve".to_string()]));

		// ...or one comma-separated string.
		assert_eq!(
			parse_tshock_players(r#"{"status": "200", "players": "WAI, Big Alex"}"#),
			Some(vec!["WAI".to_string(), "Big Alex".to_string()])
		);
		assert_eq!(parse_tshock_players(r#"{"status": "200", "players": ""}"#), Some(vec![]));
	}

	#[test]
	fn tshock_errors_are_not_an_empty_server() {
		// "Couldn't tell" must never look like "nobody online".
		assert_eq!(parse_tshock_players(r#"{"status": "403", "error": "Not authorized."}"#), None);
		assert_eq!(parse_tshock_players(r#"{"status": "200"}"#), None);
		assert_eq!(parse_tshock_players("<html>oops</html>"), None);
		assert_eq!(parse_tshock_players(""), None);
	}

	#[tokio::test]
	async fn tshock_players_over_http() {
		let (port, server) = fake_tshock(r#"{"status": "200", "players": [{"nickname": "Steve"}]}"#).await;
		assert_eq!(tshock_players(port, "abc123").await.unwrap(), vec!["Steve".to_string()]);
		let request = server.await.unwrap();
		assert!(request.lines().next().unwrap().starts_with("GET /v2/server/status?token=abc123&players=true"), "{request}");
	}

	#[tokio::test]
	async fn tshock_keep_alive_does_not_hang() {
		// What your TShock really does: answers right away with Content-Length,
		// then keeps the socket open ("Connection: Keep-Alive").
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(async move {
			let (mut socket, _) = listener.accept().await.unwrap();
			let mut buf = vec![0; 4096];
			let _ = socket.read(&mut buf).await.unwrap();
			let body = r#"{"status": "200", "players": [{"nickname": "WAI", "group": "guest"}]}"#;
			let reply = format!(
				"HTTP/1.1 200 Made by Jonas Gauffin\r\nContent-Length: {}\r\nConnection: Keep-Alive\r\n\r\n{body}",
				body.len()
			);
			socket.write_all(reply.as_bytes()).await.unwrap();
			tokio::time::sleep(Duration::from_secs(30)).await; // never closes on its own
		});

		let started = Instant::now();
		assert_eq!(tshock_players(port, "abc123").await.unwrap(), vec!["WAI".to_string()]);
		assert!(started.elapsed() < Duration::from_secs(1), "waited {:?} for a close that never comes", started.elapsed());
	}

	#[test]
	fn knows_when_a_response_is_complete() {
		assert!(!response_complete(b""));
		assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n")); // headers not done
		assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc")); // body not done
		assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde"));
		assert!(response_complete(b"HTTP/1.1 200 OK\r\ncontent-length:0\r\n\r\n"));
		// No length given: only the server closing ends it.
		assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\nabc"));
	}

	#[tokio::test]
	async fn tshock_off_rejects_missing_or_unsafe_tokens() {
		assert!(tshock_off(1, "").await.is_err());
		// '&' would sneak extra parameters into the URL.
		assert!(tshock_off(1, "abc&nosave=true").await.is_err());
		assert!(tshock_off(1, "has space").await.is_err());
	}
}
