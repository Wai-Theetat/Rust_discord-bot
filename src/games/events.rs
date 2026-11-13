//! Turn one line of server console output into an event.
//!
//! Every line is untrusted: players' chat goes into the same log, so a player
//! can type text that looks like a server message. The parsers below only
//! accept lines in the server's own format, and validate player names.

use super::config::GameKind;

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
	/// The server finished booting and accepts players.
	Ready,
	Joined(String),
	Left(String),
}

pub fn parse_line(kind: GameKind, line: &str) -> Option<Event> {
	let line = line.trim();
	match kind {
		GameKind::Minecraft => parse_minecraft(line),
		GameKind::Terraria => parse_terraria(line),
	}
}

// ---------- Minecraft ----------

/// Lines look like: `[02:40:28] [Server thread/INFO]: <message>`
fn parse_minecraft(line: &str) -> Option<Event> {
	// Split at the FIRST "]: ". It always ends the server's own prefix.
	// (Splitting at the last one would let a player type "x]: Eve joined the game".)
	let (prefix, message) = line.split_once("]: ")?;

	// Join/leave/ready messages come from the main server thread, untagged.
	// Mod messages look like "[Server thread/INFO] [IC2]: ...", so they fail here.
	if !prefix.ends_with("[Server thread/INFO") {
		return None;
	}

	// "<Steve> ..." is chat. Anything a player typed is ignored.
	if message.starts_with('<') {
		return None;
	}

	if message.starts_with("Done (") && message.contains("For help") {
		return Some(Event::Ready);
	}
	if let Some(name) = message.strip_suffix(" joined the game") {
		return is_valid_mc_name(name).then(|| Event::Joined(name.to_string()));
	}
	if let Some(name) = message.strip_suffix(" left the game") {
		return is_valid_mc_name(name).then(|| Event::Left(name.to_string()));
	}
	None
}

/// Minecraft names: 3-16 characters, letters, digits and underscore only.
fn is_valid_mc_name(name: &str) -> bool {
	(3..=16).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------- Terraria (TShock) ----------

/// Terraria lines have no prefix. The input prompt ": " may be glued to the front.
fn parse_terraria(line: &str) -> Option<Event> {
	let message = line.strip_prefix(": ").unwrap_or(line);

	if message == "Server started" {
		return Some(Event::Ready);
	}
	if let Some(name) = terraria_join_name(message) {
		return is_valid_terraria_name(name).then(|| Event::Joined(name.to_string()));
	}
	if let Some(name) = message.strip_suffix(" has left.") {
		return is_valid_terraria_name(name).then(|| Event::Left(name.to_string()));
	}
	None
}

/// The name in a TShock join line. TShock has several join formats, and most
/// add the player's country (from its GeoIP database), but the leave line
/// never does. So the country is removed here, or a rejoin would leave the
/// old "WAI (N/A)" behind next to a new "WAI (Thailand)".
///   "WAI (Thailand) has joined."
///   "WAI (Thailand) from 'default' group joined. (1/8)"
///   "WAI (Thailand) from 'default' group from 'x' joined. (1/8)"
///   "WAI has joined. IP: 1.2.3.4"
///   "WAI has joined."
fn terraria_join_name(message: &str) -> Option<&str> {
	if let Some(name) = message.strip_suffix(" has joined.") {
		return Some(without_country(name));
	}
	if let Some((name, _ip)) = message.split_once(" has joined. IP: ") {
		return Some(without_country(name));
	}
	if message.contains(" group") && message.contains(" joined. (") && message.ends_with(')') {
		let (name, _) = message.split_once(" from '")?;
		return Some(without_country(name));
	}
	None
}

/// "WAI (Thailand)" -> "WAI". Leaves names without a trailing "(...)" alone.
fn without_country(name: &str) -> &str {
	match name.rfind(" (") {
		Some(start) if name.ends_with(')') => &name[..start],
		_ => name,
	}
}

/// Terraria allows spaces and most characters, up to 20. Reject what chat looks like.
fn is_valid_terraria_name(name: &str) -> bool {
	let length = name.chars().count();
	(1..=20).contains(&length) && name.trim() == name && !name.contains(['<', '>', ':'])
}

#[cfg(test)]
mod tests {
	use super::*;

	fn mc(line: &str) -> Option<Event> {
		parse_line(GameKind::Minecraft, line)
	}

	fn tr(line: &str) -> Option<Event> {
		parse_line(GameKind::Terraria, line)
	}

	fn joined(name: &str) -> Option<Event> {
		Some(Event::Joined(name.to_string()))
	}

	fn left(name: &str) -> Option<Event> {
		Some(Event::Left(name.to_string()))
	}

	// ----- Minecraft: real lines -----

	#[test]
	fn mc_ready_from_real_gtnh_log() {
		assert_eq!(
			mc(r#"[02:40:28] [Server thread/INFO]: Done (3.669s)! For help, type "help" or "?""#),
			Some(Event::Ready)
		);
	}

	#[test]
	fn mc_join_and_leave() {
		assert_eq!(mc("[02:41:10] [Server thread/INFO]: Steve joined the game"), joined("Steve"));
		assert_eq!(mc("[02:45:33] [Server thread/INFO]: Steve left the game"), left("Steve"));
		assert_eq!(mc("[02:41:10] [Server thread/INFO]: Mr_Bob_123 joined the game"), joined("Mr_Bob_123"));
	}

	#[test]
	fn mc_handles_windows_line_endings() {
		assert_eq!(mc("[02:41:10] [Server thread/INFO]: Steve joined the game\r\n"), joined("Steve"));
	}

	#[test]
	fn mc_ignores_ordinary_log_lines() {
		assert_eq!(mc("[02:38:45] [Server thread/INFO]: Starting minecraft server version 1.7.10"), None);
		assert_eq!(mc("[02:40:58] [Server thread/INFO]: Saving worlds"), None);
		assert_eq!(mc("[0.032s][warning][cds] Archived non-system classes are disabled"), None);
		assert_eq!(mc(""), None);
	}

	// ----- Minecraft: spoofing attempts -----

	#[test]
	fn mc_chat_cannot_fake_a_join() {
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: <Steve> Alex joined the game"), None);
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: <Steve> Alex left the game"), None);
	}

	#[test]
	fn mc_chat_cannot_fake_ready() {
		assert_eq!(mc(r#"[02:42:00] [Server thread/INFO]: <Steve> Done (1s)! For help, type "help""#), None);
	}

	#[test]
	fn mc_extra_bracket_colon_in_chat_cannot_fake_a_join() {
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: <Bob> x]: Eve joined the game"), None);
	}

	#[test]
	fn mc_say_command_cannot_fake_a_join() {
		// `/say Alex joined the game` is logged as "[Server] Alex joined the game".
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: [Server] Alex joined the game"), None);
	}

	#[test]
	fn mc_mod_threads_and_tags_are_ignored() {
		assert_eq!(mc("[02:42:00] [Server thread/INFO] [SomeMod]: Steve joined the game"), None);
		assert_eq!(mc("[02:42:00] [pool-5-thread-1/INFO]: Steve joined the game"), None);
		assert_eq!(mc("[02:42:00] [Server thread/WARN]: Steve joined the game"), None);
	}

	#[test]
	fn mc_rejects_invalid_names() {
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: Al joined the game"), None); // too short
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: ThisNameIsWayTooLong joined the game"), None);
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: Two Words joined the game"), None);
		assert_eq!(mc("[02:42:00] [Server thread/INFO]: bad-name joined the game"), None);
	}

	#[test]
	fn mc_name_rule() {
		assert!(is_valid_mc_name("Abc"));
		assert!(is_valid_mc_name("abcdefghijklmnop")); // exactly 16
		assert!(!is_valid_mc_name("abcdefghijklmnopq")); // 17
		assert!(!is_valid_mc_name("Ste ve"));
		assert!(!is_valid_mc_name("Stéve"));
	}

	// ----- Terraria -----

	#[test]
	fn tr_ready_with_and_without_prompt() {
		// From the real TShock boot: the ": " prompt is glued to the front.
		assert_eq!(tr(": Server started"), Some(Event::Ready));
		assert_eq!(tr("Server started"), Some(Event::Ready));
	}

	#[test]
	fn tr_join_and_leave() {
		assert_eq!(tr("Steve has joined."), joined("Steve"));
		assert_eq!(tr("Steve has left."), left("Steve"));
		assert_eq!(tr("Big Steve has joined."), joined("Big Steve")); // spaces are allowed
	}

	#[test]
	fn tr_join_drops_the_country_tag() {
		// Your bug: localhost shows (N/A), your public IP shows (Thailand).
		assert_eq!(tr("WAI (N/A) has joined."), joined("WAI"));
		assert_eq!(tr("WAI (Thailand) has joined."), joined("WAI"));
		assert_eq!(tr("Big Steve (United States) has joined."), joined("Big Steve"));
	}

	#[test]
	fn tr_other_tshock_join_formats() {
		assert_eq!(tr("WAI (Thailand) from 'default' group joined. (1/8)"), joined("WAI"));
		assert_eq!(tr("WAI (Thailand) from 'default' group from 'superadmin' joined. (1/8)"), joined("WAI"));
		assert_eq!(tr("WAI has joined. IP: 203.0.113.7"), joined("WAI"));
	}

	#[test]
	fn tr_rejoin_from_another_ip_leaves_one_player() {
		// The exact sequence you saw, replayed through a player list.
		let mut players = std::collections::BTreeSet::new();
		for line in ["WAI (N/A) has joined.", "WAI has left.", "WAI (Thailand) has joined."] {
			match tr(line) {
				Some(Event::Joined(name)) => {
					players.insert(name);
				}
				Some(Event::Left(name)) => {
					players.remove(&name);
				}
				other => panic!("{line:?} -> {other:?}"),
			}
		}
		assert_eq!(players.into_iter().collect::<Vec<_>>(), ["WAI"]);
	}

	#[test]
	fn tr_ignores_ordinary_lines() {
		assert_eq!(tr("Listening on port 7777"), None);
		assert_eq!(tr("Settling liquids 42%"), None);
		assert_eq!(tr(": Login before join enabled."), None);
	}

	#[test]
	fn tr_chat_cannot_fake_events() {
		assert_eq!(tr("<Steve> Alex has joined."), None);
		assert_eq!(tr("<Steve> Server started"), None);
		assert_eq!(tr("Steve: Alex has left."), None);
	}

	#[test]
	fn tr_rejects_bad_names() {
		assert_eq!(tr(" has joined."), None); // empty
		assert_eq!(tr("ThisNameIsLongerThan20 has joined."), None);
	}

	#[test]
	fn kind_selects_the_parser() {
		// A Terraria line means nothing to the Minecraft parser, and the other way round.
		assert_eq!(mc("Steve has joined."), None);
		assert_eq!(tr("[02:41:10] [Server thread/INFO]: Steve joined the game"), None);
	}
}
