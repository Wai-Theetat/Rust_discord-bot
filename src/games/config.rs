use serde::{Deserialize,};
use std::collections::{HashMap, };

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GameKind {
	Minecraft,
	Terraria,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GameConfig {
	pub kind: GameKind,
	pub dir: String,
	pub start : Vec<String>,
	pub port: u16,
	pub stop_timeout_secs: u64,
	/// Terraria (TShock) only: REST API port used to stop the server. Default 7878.
	#[serde(default)]
	pub rest_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
	pub games: HashMap<String, GameConfig>,
}


/// Parse config text. Separate from file reading so tests don't need files.
pub fn parse(text: &str) -> Result<Config, String> {
	let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
	validate(&config)?;
	Ok(config)
}

pub fn load(path: &str) -> Result<Config, String> {
	let text = std::fs::read_to_string(path)
		.map_err(|e| format!("Could not read {path}: {e}"))?;
	parse(&text)
}

fn normalize_dir(dir: &str) -> String {
	dir.replace('\\', "/")
		.to_lowercase()
		.trim_end_matches('/')
		.to_string()
}

fn validate(config: &Config) -> Result<(), String> {
	let mut ports: HashMap<u16, &String> = HashMap::new();
	let mut dirs: HashMap<String, &String> = HashMap::new();
	
	for (name, game) in &config.games {
		if let Some(other) = ports.insert(game.port, name) {
			return Err(format!(
				"'{name}' and '{other}' both use port {}",
				game.port
			));
		}
	
		let normalized_dir = normalize_dir(&game.dir);
		
		if let Some(other) = dirs.insert(normalized_dir, name) {
			return Err(format!(
				"'{name}' and '{other}' both use directory '{}'",
				game.dir
			))
		}
	
	}

	Ok(())
}


#[cfg(test)]
mod tests {
	use super::*;

	/// Build the TOML for one game, so each test only spells out what it cares about.
	fn game(name: &str, port: u16, dir: &str) -> String {
		format!(
			r#"
			[games.{name}]
			kind = "minecraft"
			dir = '{dir}'
			start = ["java", "-jar", "server.jar", "nogui"]
			port = {port}
			stop_timeout_secs = 180
			"#
		)
	}

	#[test]
	fn parses_valid_config() {
		let text = r#"
		[games.gtnh]
		kind = "minecraft"
		dir = "D:/servers/gtnh"
		start = ["java", "-Xmx6G", "@java9args.txt", "-jar", "lwjgl3ify-forgePatches.jar", "nogui"]
		port = 25565
		stop_timeout_secs = 180

		[games.terraria]
		kind = "terraria"
		dir = "D:/servers/terraria"
		start = ["TerrariaServer.exe", "-config", "serverconfig.txt"]
		port = 7777
		stop_timeout_secs = 30
		"#;
		let config = parse(text).expect("valid config should parse");
		assert_eq!(config.games.len(), 2);

		let gtnh = &config.games["gtnh"];
		assert_eq!(gtnh.kind, GameKind::Minecraft);
		assert_eq!(gtnh.port, 25565);
		assert_eq!(gtnh.start[0], "java");
		assert_eq!(gtnh.start.last().unwrap(), "nogui");

		assert_eq!(config.games["terraria"].kind, GameKind::Terraria);
	}

	#[test]
	fn rejects_duplicate_port() {
		let text = game("gtnh", 25565, "D:/a") + &game("creative", 25565, "D:/b");
		let error = parse(&text).unwrap_err();

		// The error must name both games, whichever one the loop saw first.
		assert!(error.contains("gtnh"), "{error}");
		assert!(error.contains("creative"), "{error}");
		assert!(error.contains("25565"), "{error}");
	}

	#[test]
	fn rejects_duplicate_dir_after_normalizing() {
		// Same folder on Windows, written two different ways.
		let text = game("one", 1, "D:/servers/gtnh") + &game("two", 2, r"d:\Servers\GTNH\");
		let error = parse(&text).unwrap_err();
		assert!(error.contains("directory"), "{error}");
	}

	#[test]
	fn allows_different_ports_and_dirs() {
		let text = game("one", 1, "D:/a") + &game("two", 2, "D:/b");
		assert!(parse(&text).is_ok());
	}

	#[test]
	fn rejects_unknown_kind() {
		let text = game("valheim", 2456, "D:/v").replace("minecraft", "valheim");
		let error = parse(&text).unwrap_err();
		assert!(error.contains("unknown variant"), "{error}");
	}

	#[test]
	fn rejects_missing_field() {
		let text = game("gtnh", 25565, "D:/a").replace("port = 25565\n", "");
		let error = parse(&text).unwrap_err();
		assert!(error.contains("missing field `port`"), "{error}");
	}

	#[test]
	fn normalize_dir_unifies_slashes_and_case() {
		assert_eq!(normalize_dir(r"D:\Servers\GTNH\"), "d:/servers/gtnh");
		assert_eq!(normalize_dir("d:/servers/gtnh"), "d:/servers/gtnh");
	}

	#[test]
	fn load_reports_missing_file() {
		let error = load("does-not-exist.toml").unwrap_err();
		assert!(error.contains("does-not-exist.toml"), "{error}");
	}
}
