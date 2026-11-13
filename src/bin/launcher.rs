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

use std::net::SocketAddr;
use std::sync::Arc;

use bot_discord::games::launcher;
use bot_discord::games::config;
use bot_discord::games::manager::GameManager;

const DEFAULT_ADDR: &str = "127.0.0.1:7979";
const DEFAULT_GAMES_CONFIG: &str = "games.toml";

#[tokio::main]
async fn main() {
	let _ = dotenvy::dotenv();

	let token = std::env::var("LAUNCHER_TOKEN").unwrap_or_default();
	if token.len() < launcher::MIN_TOKEN_LEN {
		fail(&format!(
			"LAUNCHER_TOKEN is missing or shorter than {} characters. Add it to .env.",
			launcher::MIN_TOKEN_LEN
		));
	}

	let addr = std::env::var("LAUNCHER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
	let addr: SocketAddr = addr
		.parse()
		.unwrap_or_else(|_| fail(&format!("LAUNCHER_ADDR {addr:?} is not an address like 127.0.0.1:7979")));
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

	let app = launcher::router(Arc::clone(&manager), token);
	if let Err(error) = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await {
		eprintln!("[launcher] server error: {error}");
	}

	println!("[launcher] shutting down: stopping running servers (GTNH can take a few minutes)...");
	launcher::stop_all(&manager).await;
	println!("[launcher] bye");
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
