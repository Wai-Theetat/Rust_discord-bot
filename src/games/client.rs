//! What the bot uses to talk to the game launcher over HTTP.

use std::fmt;
use std::time::Duration;

use reqwest::{Client, Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::launcher::{CommandRequest, ErrorBody, GameInfo, GameState};
use super::manager::{ConsoleSnapshot, StartOutcome, StopOutcome};

/// The game list. Short, because autocomplete must answer within 3 seconds.
const LIST_TIMEOUT: Duration = Duration::from_secs(2);
/// One game's status. The launcher also measures RAM/CPU and may ask TShock
/// who's online, so give it a little more room.
const STATE_TIMEOUT: Duration = Duration::from_secs(5);
/// Starting only launches the process (the bot polls for "ready" afterwards).
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Added on top of a game's stop_timeout_secs (covers the force-kill wait too).
const STOP_EXTRA: Duration = Duration::from_secs(60);
/// One console command. TShock REST commands answer within about a second.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

pub struct LauncherClient {
	base_url: String,
	token: String,
	http: Client,
}

#[derive(Debug, PartialEq)]
pub enum LauncherError {
	/// Couldn't connect, or no answer in time. Usually: the launcher isn't running.
	Unreachable(String),
	/// The launcher rejected LAUNCHER_TOKEN.
	Unauthorized,
	NotFound,
	/// The launcher answered with an error (e.g. "could not start java").
	Failed(String),
}

impl fmt::Display for LauncherError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Unreachable(why) => write!(f, "The game launcher on the PC isn't answering ({why}). Is it running?"),
			Self::Unauthorized => write!(f, "The game launcher rejected the bot's LAUNCHER_TOKEN."),
			Self::NotFound => write!(f, "The game launcher doesn't know that game."),
			Self::Failed(message) => write!(f, "{message}"),
		}
	}
}

impl LauncherClient {
	pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
		Self {
			base_url: base_url.into().trim_end_matches('/').to_string(),
			token: token.into(),
			http: Client::new(),
		}
	}

	pub async fn list(&self) -> Result<Vec<GameInfo>, LauncherError> {
		self.call(Method::GET, "/games".to_string(), LIST_TIMEOUT).await
	}

	pub async fn state(&self, name: &str) -> Result<GameState, LauncherError> {
		self.call(Method::GET, game_path(name, "")?, STATE_TIMEOUT).await
	}

	pub async fn start(&self, name: &str) -> Result<StartOutcome, LauncherError> {
		self.call(Method::POST, game_path(name, "/start")?, START_TIMEOUT).await
	}

	pub async fn stop(&self, name: &str, stop_timeout_secs: u64) -> Result<StopOutcome, LauncherError> {
		let timeout = Duration::from_secs(stop_timeout_secs) + STOP_EXTRA;
		self.call(Method::POST, game_path(name, "/stop")?, timeout).await
	}

	pub async fn restart(&self, name: &str, stop_timeout_secs: u64) -> Result<StartOutcome, LauncherError> {
		let timeout = Duration::from_secs(stop_timeout_secs) + STOP_EXTRA + START_TIMEOUT;
		self.call(Method::POST, game_path(name, "/restart")?, timeout).await
	}

	/// Console lines newer than `after` (for the console windows).
	pub async fn console(&self, name: &str, after: u64) -> Result<ConsoleSnapshot, LauncherError> {
		self.call(Method::GET, game_path(name, &format!("/console?after={after}"))?, STATE_TIMEOUT).await
	}

	/// Type one command into the server's console.
	pub async fn command(&self, name: &str, command: &str) -> Result<(), LauncherError> {
		let body = CommandRequest { command: command.to_string() };
		self.send(Method::POST, game_path(name, "/command")?, COMMAND_TIMEOUT, Some(&body)).await
	}

	async fn call<T: DeserializeOwned>(&self, method: Method, path: String, timeout: Duration) -> Result<T, LauncherError> {
		self.send::<T, ()>(method, path, timeout, None).await
	}

	async fn send<T: DeserializeOwned, B: Serialize>(
		&self,
		method: Method,
		path: String,
		timeout: Duration,
		body: Option<&B>,
	) -> Result<T, LauncherError> {
		let mut request = self
			.http
			.request(method, format!("{}{path}", self.base_url))
			.bearer_auth(&self.token)
			.timeout(timeout);
		if let Some(body) = body {
			request = request.json(body);
		}
		let response = request
			.send()
			.await
			.map_err(|error| LauncherError::Unreachable(describe(&error)))?;

		match response.status() {
			status if status.is_success() => response
				.json::<T>()
				.await
				.map_err(|error| LauncherError::Failed(format!("Unexpected reply from the game launcher: {error}"))),
			StatusCode::UNAUTHORIZED => Err(LauncherError::Unauthorized),
			StatusCode::NOT_FOUND => Err(LauncherError::NotFound),
			status => {
				let message = match response.json::<ErrorBody>().await {
					Ok(body) => body.error,
					Err(_) => format!("game launcher error {status}"),
				};
				Err(LauncherError::Failed(message))
			}
		}
	}
}

/// `/games/<name><suffix>`. Names come from users, so only plain names are
/// allowed: no '/', '..', '?', spaces or anything else that could change the URL.
fn game_path(name: &str, suffix: &str) -> Result<String, LauncherError> {
	let plain = !name.is_empty()
		&& name.len() <= 64
		&& name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
	if plain {
		Ok(format!("/games/{name}{suffix}"))
	} else {
		Err(LauncherError::NotFound)
	}
}

/// A short reason for Discord. Never includes headers, so never the token.
fn describe(error: &reqwest::Error) -> String {
	if error.is_timeout() {
		"no answer in time".to_string()
	} else if error.is_connect() {
		"could not connect".to_string()
	} else {
		"request failed".to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_plain_names_reach_the_url() {
		assert_eq!(game_path("gtnh", "/start"), Ok("/games/gtnh/start".to_string()));
		assert_eq!(game_path("gtnh-creative_2", ""), Ok("/games/gtnh-creative_2".to_string()));
		for bad in ["", "../games", "a/b", "gtnh?x=1", "two words", "gtnh%2F", "ทดสอบ"] {
			assert_eq!(game_path(bad, ""), Err(LauncherError::NotFound), "{bad:?}");
		}
	}
}
