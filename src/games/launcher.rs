//! The game launcher's HTTP API. It runs on Windows next to the game servers,
//! and the bot (in Docker) calls it.
//!
//!   GET  /games                 -> [GameInfo]          all configured games
//!   GET  /games/{name}          -> GameState           info + live status
//!   POST /games/{name}/start    -> StartOutcome
//!   POST /games/{name}/stop     -> StopOutcome
//!   POST /games/{name}/restart  -> StartOutcome
//!
//! Every request needs `Authorization: Bearer <LAUNCHER_TOKEN>`. The API can only
//! start/stop/check games from games.toml: there is no "run this command" route,
//! so even a leaked token can't run arbitrary programs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use super::manager::{Game, GameManager, StartOutcome, Status, StopOutcome};

/// The launcher refuses to start with a shorter token than this.
pub const MIN_TOKEN_LEN: usize = 16;

// ---------- JSON types (shared with the client) ----------

/// The fixed facts about one game, from games.toml.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameInfo {
	pub name: String,
	pub port: u16,
	pub stop_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameState {
	pub info: GameInfo,
	pub status: Status,
	/// RAM/CPU numbers while it runs. None when stopped or if they couldn't be read.
	#[serde(default)]
	pub resources: Option<Resources>,
}

/// Measured on the PC by the launcher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resources {
	/// RAM used by the server (its process plus any child processes), in bytes.
	pub server_memory: u64,
	/// CPU used by the server, as % of the whole PC (0-100), since the previous check.
	pub server_cpu_percent: f32,
	/// RAM in use on the whole PC, in bytes.
	pub pc_memory_used: u64,
	pub pc_memory_total: u64,
	/// When the server process started, as Unix seconds.
	pub started_unix: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
	pub error: String,
}

impl GameInfo {
	fn of(game: &Game) -> Self {
		Self {
			name: game.name.clone(),
			port: game.config.port,
			stop_timeout_secs: game.config.stop_timeout_secs,
		}
	}
}

// ---------- router ----------

#[derive(Clone)]
struct AppState {
	manager: Arc<GameManager>,
	token: Arc<str>,
	/// Kept between requests: CPU % is measured as the change since the last
	/// refresh. None if the OS info couldn't be opened (then no RAM/CPU shown).
	system: Arc<Mutex<Option<System>>>,
}

pub fn router(manager: Arc<GameManager>, token: String) -> Router {
	let state = AppState { manager, token: token.into(), system: Arc::new(Mutex::new(System::new().ok())) };
	Router::new()
		.route("/games", get(list_games))
		.route("/games/{name}", get(game_state))
		.route("/games/{name}/start", post(start_game))
		.route("/games/{name}/stop", post(stop_game))
		.route("/games/{name}/restart", post(restart_game))
		// Runs before every route above.
		.layer(middleware::from_fn_with_state(state.clone(), require_token))
		.with_state(state)
}

/// Stop every running server, all at the same time. Used when the launcher shuts down.
pub async fn stop_all(manager: &GameManager) {
	let mut tasks = tokio::task::JoinSet::new();
	for game in manager.all() {
		let game = Arc::clone(game);
		tasks.spawn(async move {
			let outcome = game.stop().await;
			(game.name.clone(), outcome)
		});
	}
	while let Some(result) = tasks.join_next().await {
		match result {
			Ok((_, Ok(StopOutcome::NotRunning))) => {}
			Ok((name, Ok(outcome))) => println!("[launcher] {name}: {outcome:?}"),
			Ok((name, Err(error))) => eprintln!("[launcher] {name}: could not stop: {error}"),
			Err(error) => eprintln!("[launcher] stop task failed: {error}"),
		}
	}
}

// ---------- auth ----------

async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
	let given = request
		.headers()
		.get(header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or("");

	if !token_matches(given, &state.token) {
		return ApiError::new(StatusCode::UNAUTHORIZED, "missing or wrong launcher token").into_response();
	}
	next.run(request).await
}

/// Compare without stopping at the first different byte, so response timing
/// doesn't reveal how much of a guessed token was right.
fn token_matches(given: &str, expected: &str) -> bool {
	let (given, expected) = (given.as_bytes(), expected.as_bytes());
	if given.len() != expected.len() || expected.is_empty() {
		return false;
	}
	given.iter().zip(expected).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

// ---------- handlers ----------

async fn list_games(State(state): State<AppState>) -> Json<Vec<GameInfo>> {
	let mut games: Vec<GameInfo> = state.manager.all().map(|game| GameInfo::of(game)).collect();
	games.sort_by(|a, b| a.name.cmp(&b.name));
	Json(games)
}

async fn game_state(State(state): State<AppState>, Path(name): Path<String>) -> Result<Json<GameState>, ApiError> {
	let game = find(&state, &name)?;
	let mut status = game.status().await;
	// Terraria: the real player list from TShock beats parsing its log.
	if let Some(live) = game.live_players().await
		&& let Status::Running { players, .. } = &mut status
	{
		*players = live;
	}
	let resources = match game.process().await {
		Some((pid, started_unix)) => measure(&state.system, pid, started_unix).await,
		None => None,
	};
	Ok(Json(GameState { info: GameInfo::of(&game), status, resources }))
}

// ---------- RAM / CPU ----------

/// Read RAM/CPU for a running server. Reading every process takes a moment,
/// so it runs on a blocking thread instead of holding up other requests.
async fn measure(system: &Arc<Mutex<Option<System>>>, pid: u32, started_unix: u64) -> Option<Resources> {
	let system = Arc::clone(system);
	tokio::task::spawn_blocking(move || {
		let mut guard = system.lock().ok()?;
		let system = guard.as_mut()?;
		system.refresh_memory();
		system.refresh_processes_specifics(
			ProcessesToUpdate::All,
			true,
			ProcessRefreshKind::nothing().with_memory().with_cpu(),
		);
		system.process(Pid::from_u32(pid))?; // already gone

		let parents = system
			.processes()
			.iter()
			.map(|(pid, process)| (pid.as_u32(), process.parent().map(Pid::as_u32)));
		let (mut memory, mut cpu) = (0u64, 0f32);
		for member in process_tree(parents, pid) {
			if let Some(process) = system.process(Pid::from_u32(member)) {
				memory += process.memory();
				cpu += process.cpu_usage();
			}
		}

		// cpu_usage is per core (a busy 8-core PC = 800%), so divide by cores.
		let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
		Some(Resources {
			server_memory: memory,
			server_cpu_percent: (cpu / cores).clamp(0.0, 100.0),
			pc_memory_used: system.used_memory(),
			pc_memory_total: system.total_memory(),
			started_unix,
		})
	})
	.await
	.ok()
	.flatten()
}

/// `root` and everything it started (children, grandchildren, ...). A server
/// launched through cmd/a .bat has the real work in a child process.
fn process_tree(processes: impl Iterator<Item = (u32, Option<u32>)>, root: u32) -> Vec<u32> {
	let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
	for (pid, parent) in processes {
		if let Some(parent) = parent {
			children.entry(parent).or_default().push(pid);
		}
	}
	// `seen` guards against loops (Windows can reuse old process ids).
	let mut seen = HashSet::from([root]);
	let mut tree = vec![root];
	let mut next = 0;
	while next < tree.len() {
		for &child in children.get(&tree[next]).map(Vec::as_slice).unwrap_or(&[]) {
			if seen.insert(child) {
				tree.push(child);
			}
		}
		next += 1;
	}
	tree
}

// Start/stop/restart run in their own task. If the bot gives up waiting and
// closes the connection, axum drops the handler, but the task keeps going, so
// a stop always finishes (including the force-kill fallback).

async fn start_game(State(state): State<AppState>, Path(name): Path<String>) -> Result<Json<StartOutcome>, ApiError> {
	let game = find(&state, &name)?;
	run_to_completion(async move { game.start().await }).await
}

async fn stop_game(State(state): State<AppState>, Path(name): Path<String>) -> Result<Json<StopOutcome>, ApiError> {
	let game = find(&state, &name)?;
	run_to_completion(async move { game.stop().await }).await
}

async fn restart_game(State(state): State<AppState>, Path(name): Path<String>) -> Result<Json<StartOutcome>, ApiError> {
	let game = find(&state, &name)?;
	run_to_completion(async move { game.restart().await }).await
}

async fn run_to_completion<T, F>(work: F) -> Result<Json<T>, ApiError>
where
	T: Send + 'static,
	F: Future<Output = Result<T, String>> + Send + 'static,
{
	match tokio::spawn(work).await {
		Ok(Ok(value)) => Ok(Json(value)),
		Ok(Err(message)) => Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message)),
		Err(error) => Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("task failed: {error}"))),
	}
}

fn find(state: &AppState, name: &str) -> Result<Arc<Game>, ApiError> {
	state
		.manager
		.get(name)
		.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no game named {name:?}")))
}

// ---------- errors ----------

struct ApiError {
	status: StatusCode,
	message: String,
}

impl ApiError {
	fn new(status: StatusCode, message: impl Into<String>) -> Self {
		Self { status, message: message.into() }
	}
}

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		(self.status, Json(ErrorBody { error: self.message })).into_response()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn process_tree_includes_children_and_grandchildren() {
		// 10 = cmd.exe, 11 = java.exe it started, 12 = a helper java started.
		// 20 and 21 are unrelated.
		let processes = [(10, Some(1)), (11, Some(10)), (12, Some(11)), (20, Some(1)), (21, Some(20))];
		let mut tree = process_tree(processes.into_iter(), 10);
		tree.sort_unstable();
		assert_eq!(tree, [10, 11, 12]);

		assert_eq!(process_tree(processes.into_iter(), 12), [12]);
	}

	#[test]
	fn process_tree_survives_loops() {
		// A reused pid can make a cycle: 5 -> 6 -> 5.
		let mut tree = process_tree([(5, Some(6)), (6, Some(5))].into_iter(), 5);
		tree.sort_unstable();
		assert_eq!(tree, [5, 6]);
	}

	#[test]
	fn token_comparison() {
		assert!(token_matches("abcdef", "abcdef"));
		assert!(!token_matches("abcdeg", "abcdef"));
		assert!(!token_matches("abc", "abcdef"));
		assert!(!token_matches("", "abcdef"));
		// An empty expected token must never match, not even an empty header.
		assert!(!token_matches("", ""));
	}
}

/// The whole chain the bot uses: LauncherClient -> HTTP -> router -> GameManager -> process.
#[cfg(all(test, windows))]
mod end_to_end {
	use std::collections::HashMap;
	use std::time::Duration;

	use super::*;
	use crate::games::client::{LauncherClient, LauncherError};
	use crate::games::config::{Config, GameConfig, GameKind};

	const TOKEN: &str = "test-token-0123456789";

	/// A fake Minecraft server that quits on `stop`.
	fn fake_game_config(test_name: &str) -> GameConfig {
		let dir = std::env::temp_dir().join(format!("bot_discord_launcher_{test_name}_{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let script = "@echo off\r\n\
			echo [00:00:00] [Server thread/INFO]: Done (1.0s)! For help, type \"help\"\r\n\
			echo [00:00:01] [Server thread/INFO]: Steve joined the game\r\n\
			:loop\r\n\
			set /p cmd=\r\n\
			if \"%cmd%\"==\"stop\" exit /b 0\r\n\
			goto loop\r\n";
		std::fs::write(dir.join("server.bat"), script).unwrap();
		GameConfig {
			kind: GameKind::Minecraft,
			dir: dir.to_string_lossy().into_owned(),
			start: vec!["cmd".into(), "/c".into(), r".\server.bat".into()],
			port: 25565,
			stop_timeout_secs: 5,
			rest_port: None,
		}
	}

	/// Start a launcher on a random free port. Returns its base URL.
	async fn spawn_launcher(test_name: &str) -> String {
		let mut games = HashMap::new();
		games.insert("fake".to_string(), fake_game_config(test_name));
		let manager = Arc::new(GameManager::new(Config { games }));

		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		tokio::spawn(async move {
			axum::serve(listener, router(manager, TOKEN.to_string())).await.unwrap();
		});
		url
	}

	async fn wait_until_running(client: &LauncherClient) -> Status {
		for _ in 0..200 {
			let status = client.state("fake").await.unwrap().status;
			if matches!(status, Status::Running { .. }) {
				return status;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		panic!("never became Running");
	}

	#[tokio::test]
	async fn full_lifecycle_over_http() {
		let url = spawn_launcher("lifecycle").await;
		let client = LauncherClient::new(&url, TOKEN);

		let games = client.list().await.unwrap();
		assert_eq!(games, vec![GameInfo { name: "fake".into(), port: 25565, stop_timeout_secs: 5 }]);
		assert_eq!(client.state("fake").await.unwrap().status, Status::Stopped { last_exit: None });

		assert_eq!(client.start("fake").await.unwrap(), StartOutcome::Started);
		let status = wait_until_running(&client).await;
		// Players survive the JSON round trip too.
		assert!(matches!(&status, Status::Running { players, .. } if players == &["Steve"]), "{status:?}");

		// Real numbers from the OS for the running cmd.exe process.
		let resources = client.state("fake").await.unwrap().resources.expect("RAM/CPU while running");
		assert!(resources.server_memory > 1024 * 1024, "server RAM looks wrong: {resources:?}");
		assert!(resources.pc_memory_total > resources.pc_memory_used && resources.pc_memory_used > 0, "{resources:?}");
		assert!((0.0..=100.0).contains(&resources.server_cpu_percent), "{resources:?}");
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		assert!(now.abs_diff(resources.started_unix) < 60, "start time looks wrong: {resources:?}");

		assert!(matches!(client.start("fake").await.unwrap(), StartOutcome::AlreadyRunning(_)));

		assert_eq!(client.stop("fake", 5).await.unwrap(), StopOutcome::Stopped);
		let Status::Stopped { last_exit: Some(exit) } = client.state("fake").await.unwrap().status else {
			panic!("not stopped");
		};
		assert!(!exit.crashed);
		assert_eq!(client.state("fake").await.unwrap().resources, None, "no RAM/CPU once stopped");

		// Restart from stopped just starts it.
		assert_eq!(client.restart("fake", 5).await.unwrap(), StartOutcome::Started);
		wait_until_running(&client).await;
		assert_eq!(client.stop("fake", 5).await.unwrap(), StopOutcome::Stopped);
	}

	#[tokio::test]
	async fn wrong_or_missing_token_is_rejected() {
		let url = spawn_launcher("auth").await;

		let good = LauncherClient::new(&url, TOKEN);
		for bad in ["wrong-token-0123456789", "", "test-token-012345678"] {
			let client = LauncherClient::new(&url, bad);
			assert_eq!(client.list().await.unwrap_err(), LauncherError::Unauthorized, "token {bad:?}");

			// Must be refused BEFORE anything happens. If it wrongly started the
			// game, stop it first, so a failing test doesn't leave a process behind.
			let result = client.start("fake").await;
			if result != Err(LauncherError::Unauthorized) {
				let _ = good.stop("fake", 5).await;
				panic!("token {bad:?} was not rejected: {result:?}");
			}
		}
		assert_eq!(good.state("fake").await.unwrap().status, Status::Stopped { last_exit: None });
	}

	#[tokio::test]
	async fn requests_without_any_auth_header_are_rejected() {
		let url = spawn_launcher("noheader").await;
		let response = reqwest::Client::new().post(format!("{url}/games/fake/start")).send().await.unwrap();
		if response.status() != reqwest::StatusCode::UNAUTHORIZED {
			let _ = LauncherClient::new(&url, TOKEN).stop("fake", 5).await;
			panic!("request without a token was not rejected: {}", response.status());
		}
	}

	#[tokio::test]
	async fn unknown_games_and_tricky_names() {
		let url = spawn_launcher("unknown").await;
		let client = LauncherClient::new(&url, TOKEN);
		assert_eq!(client.state("nope").await.unwrap_err(), LauncherError::NotFound);
		assert_eq!(client.start("../fake").await.unwrap_err(), LauncherError::NotFound);
		assert_eq!(client.start("fake/../fake").await.unwrap_err(), LauncherError::NotFound);
	}

	#[tokio::test]
	async fn launcher_not_running() {
		// A free port with nobody listening.
		let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
		let client = LauncherClient::new(format!("http://127.0.0.1:{port}"), TOKEN);
		assert!(matches!(client.list().await.unwrap_err(), LauncherError::Unreachable(_)));
	}
}
