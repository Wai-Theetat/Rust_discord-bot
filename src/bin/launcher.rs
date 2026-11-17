//! The game launcher: runs on Windows, owns the game servers, and lets the bot
//! (in Docker) start/stop/check them over HTTP.
//!
//! Run from the repo folder (so it finds .env and games.toml):
//!   cargo run --release --bin launcher
//!
//! Settings (from .env):
//!   LAUNCHER_TOKEN   required, at least 16 characters, same value as the bot's
//!   LAUNCHER_ADDR    optional, default 127.0.0.1:7979 (this PC only)
//!   GAMES_CONFIG  optional, default games.toml
//!
//! Every server that starts gets its own console window: this same program run
//! as `launcher console <name>`. You can also run that yourself to reopen one.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bot_discord::games::client::LauncherClient;
use bot_discord::games::config;
use bot_discord::games::launcher;
use bot_discord::games::manager::GameManager;

const DEFAULT_ADDR: &str = "127.0.0.1:7979";
const DEFAULT_GAMES_CONFIG: &str = "games.toml";
/// How often a console window asks for new lines.
const CONSOLE_POLL: Duration = Duration::from_millis(500);
/// How long a console window stays open after its server stopped.
const CONSOLE_LINGER: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() {
	let _ = dotenvy::dotenv();

	let args: Vec<String> = std::env::args().collect();
	match args.get(1).map(String::as_str) {
		None => run_launcher().await,
		Some("console") => match args.get(2) {
			Some(name) => console_window(name).await,
			None => fail("usage: launcher console <game name>"),
		},
		Some(other) => fail(&format!("unknown argument {other:?}. Run with no arguments, or `console <game name>`.")),
	}
}

async fn run_launcher() {
	let token = std::env::var("LAUNCHER_TOKEN").unwrap_or_default();
	if token.len() < launcher::MIN_TOKEN_LEN {
		fail(&format!(
			"LAUNCHER_TOKEN is missing or shorter than {} characters. Add it to .env.",
			launcher::MIN_TOKEN_LEN
		));
	}

	let addr = launcher_addr();
	if !addr.ip().is_loopback() {
		// Docker Desktop reaches 127.0.0.1 just fine; anything else opens the
		// launcher to the network.
		eprintln!("[launcher] WARNING: listening on {addr} makes the launcher reachable from other devices.");
	}

	let path = std::env::var("GAMES_CONFIG").unwrap_or_else(|_| DEFAULT_GAMES_CONFIG.to_string());
	let config = config::load(&path).unwrap_or_else(|error| fail(&format!("Invalid {path}: {error}")));
	let manager = Arc::new(GameManager::new(config));

	let listener = tokio::net::TcpListener::bind(addr)
		.await
		.unwrap_or_else(|error| fail(&format!("Could not listen on {addr}: {error}")));
	println!("[launcher] listening on http://{addr}");
	println!("[launcher] games: {}", manager.names().join(", "));
	println!("[launcher] press Ctrl+C to stop (running servers are saved and stopped first)");

	let app = launcher::router(Arc::clone(&manager), token, true);
	if let Err(error) = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await {
		eprintln!("[launcher] server error: {error}");
	}

	println!("[launcher] shutting down: stopping running servers (GTNH can take a few minutes)...");
	launcher::stop_all(&manager).await;
	println!("[launcher] bye");
}

fn launcher_addr() -> SocketAddr {
	let addr = std::env::var("LAUNCHER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
	addr.parse()
		.unwrap_or_else(|_| fail(&format!("LAUNCHER_ADDR {addr:?} is not an address like 127.0.0.1:7979")))
}

// ---------- console window ----------

/// `launcher console <name>`: show one server's console and send what you type.
/// Talks to the running launcher over HTTP, like the bot does.
async fn console_window(name: &str) {
	let mut addr = launcher_addr();
	// Listening on 0.0.0.0 means "every address"; connect through this PC's own.
	if addr.ip().is_unspecified() {
		addr.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
	}
	let token = std::env::var("LAUNCHER_TOKEN").unwrap_or_default();
	let client = LauncherClient::new(format!("http://{addr}"), token);

	println!("=== {name} console ===");
	println!("Type a command and press Enter (no leading /), e.g. `say hello` or `kick Bob`.");
	println!("Closing this window does NOT stop the server.\n");

	// Reading the keyboard blocks, so it gets its own thread.
	let (typed_tx, mut typed) = tokio::sync::mpsc::unbounded_channel::<String>();
	std::thread::spawn(move || {
		for line in std::io::stdin().lines() {
			let Ok(line) = line else { break };
			if typed_tx.send(line).is_err() {
				break;
			}
		}
	});

	let mut after = 0;
	let mut run_id = None;
	let mut last_error: Option<String> = None;
	loop {
		tokio::select! {
			Some(line) = typed.recv() => {
				if !line.trim().is_empty()
					&& let Err(error) = client.command(name, &line).await
				{
					println!("! {error}");
				}
			}
			_ = tokio::time::sleep(CONSOLE_POLL) => {
				let snapshot = match client.console(name, after).await {
					Ok(snapshot) => snapshot,
					Err(error) => {
						// Say it once, not twice a second.
						let error = error.to_string();
						if last_error.as_ref() != Some(&error) {
							println!("! {error}");
							last_error = Some(error);
						}
						continue;
					}
				};
				last_error = None;
				for line in &snapshot.lines {
					println!("{}", line.text);
					after = line.seq;
				}
				match (run_id, snapshot.run_id) {
					(None, Some(now)) => run_id = Some(now),
					(None, None) => {
						println!("\n{name} isn't running. A console opens by itself when it starts.");
						return linger(&mut typed).await;
					}
					// Stopped, or restarted (the new run opens its own window).
					(Some(old), now) if now != Some(old) => {
						println!("\n=== {name} is no longer running. Press Enter to close this window. ===");
						return linger(&mut typed).await;
					}
					_ => {}
				}
			}
		}
	}
}

/// Keep the window open so the last lines (a crash reason) can be read.
async fn linger(typed: &mut tokio::sync::mpsc::UnboundedReceiver<String>) {
	let _ = tokio::time::timeout(CONSOLE_LINGER, typed.recv()).await;
}

/// Ctrl+C, or the console window being closed.
async fn shutdown_signal() {
	#[cfg(windows)]
	{
		match tokio::signal::windows::ctrl_close() {
			Ok(mut close) => {
				tokio::select! {
					_ = tokio::signal::ctrl_c() => {}
					_ = close.recv() => {}
				}
			}
			Err(_) => {
				let _ = tokio::signal::ctrl_c().await;
			}
		}
	}
	#[cfg(not(windows))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
}

fn fail(message: &str) -> ! {
	eprintln!("[launcher] {message}");
	std::process::exit(1);
}
