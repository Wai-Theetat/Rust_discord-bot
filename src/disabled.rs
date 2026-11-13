//! Commands the owner switched off with `/disable`. Saved to
//! `<data_dir>/disabled_commands.json`, so they stay off after a restart.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const STATE_FILE: &str = "disabled_commands.json";

pub struct DisabledCommands {
	path: PathBuf,
	names: Mutex<BTreeSet<String>>,
}

impl DisabledCommands {
	/// Load the saved list (missing or unreadable file = nothing disabled).
	pub fn load(data_dir: &Path) -> Self {
		let path = data_dir.join(STATE_FILE);
		let names = match std::fs::read_to_string(&path) {
			Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
				eprintln!("[disabled] ignoring unreadable {}: {error}", path.display());
				BTreeSet::new()
			}),
			Err(_) => BTreeSet::new(),
		};
		Self { path, names: Mutex::new(names) }
	}

	pub fn is_disabled(&self, name: &str) -> bool {
		self.names.lock().map(|names| names.contains(name)).unwrap_or(false)
	}

	/// All disabled commands, sorted.
	pub fn list(&self) -> Vec<String> {
		self.names.lock().map(|names| names.iter().cloned().collect()).unwrap_or_default()
	}

	/// Switch `name` off or on. Returns false if it already was. Saves right away.
	pub fn set(&self, name: &str, disabled: bool) -> Result<bool, String> {
		let mut names = self.names.lock().map_err(|_| "disabled list is unavailable".to_string())?;
		let changed = if disabled { names.insert(name.to_string()) } else { names.remove(name) };
		if changed {
			save(&self.path, &names)?;
		}
		Ok(changed)
	}
}

/// Write via a temp file + rename, so a crash mid-write can't leave half a file.
fn save(path: &Path, names: &BTreeSet<String>) -> Result<(), String> {
	if let Some(dir) = path.parent() {
		std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
	}
	let text = serde_json::to_string_pretty(names).map_err(|e| e.to_string())?;
	let temp = path.with_extension("json.tmp");
	std::fs::write(&temp, text).map_err(|e| format!("could not write {}: {e}", temp.display()))?;
	std::fs::rename(&temp, path).map_err(|e| format!("could not save {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp_dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("bot_discord_disabled_{name}_{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	#[test]
	fn disable_enable_and_survive_a_restart() {
		let dir = temp_dir("restart");
		let disabled = DisabledCommands::load(&dir);
		assert!(!disabled.is_disabled("game"));

		assert_eq!(disabled.set("game", true), Ok(true));
		assert_eq!(disabled.set("game", true), Ok(false), "already off");
		assert_eq!(disabled.set("roll", true), Ok(true));
		assert!(disabled.is_disabled("game"));

		// "Restart": load again from disk.
		let reloaded = DisabledCommands::load(&dir);
		assert_eq!(reloaded.list(), ["game", "roll"]);

		assert_eq!(reloaded.set("game", false), Ok(true));
		assert_eq!(reloaded.set("game", false), Ok(false), "already on");
		assert_eq!(DisabledCommands::load(&dir).list(), ["roll"]);
	}

	#[test]
	fn bad_file_means_nothing_disabled() {
		let dir = temp_dir("corrupt");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join(STATE_FILE), "{ not json").unwrap();
		assert!(DisabledCommands::load(&dir).list().is_empty());
	}
}
