//! Minecraft "server list ping": what the multiplayer screen does to show a
//! server's version, player count and ping. Works with Minecraft 1.7 and newer
//! (GTNH is 1.7.10).
//!
//! The conversation, over one TCP connection:
//!   us -> Handshake (protocol version, address, port, "next state = status")
//!   us -> Status request
//!   server -> Status response: a JSON string
//!   us -> Ping (any 8 bytes)
//!   server -> Pong (the same 8 bytes)  <- the time this takes is the latency
//!
//! Every packet is: [length: VarInt] [packet id: VarInt] [data...]
//! A VarInt stores a number in 1-5 bytes: 7 bits per byte, and the top bit
//! says "another byte follows". Small numbers (most of them) take 1 byte.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The whole ping must finish within this.
const PING_TIMEOUT: Duration = Duration::from_secs(2);
/// A status reply is a few KB at most. Refuse anything silly.
const MAX_PACKET_LEN: usize = 64 * 1024;
/// "Any version": servers answer the status request regardless.
const PROTOCOL_ANY: i32 = -1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingInfo {
	/// Round trip of the ping packet, in milliseconds.
	pub latency_ms: u64,
	pub online: u32,
	pub max: u32,
	/// e.g. "1.7.10"
	pub version: String,
	/// The server's message of the day, without Minecraft color codes.
	pub motd: String,
}

/// Ping the Minecraft server at 127.0.0.1:`port`.
pub async fn ping(port: u16) -> Result<PingInfo, String> {
	match tokio::time::timeout(PING_TIMEOUT, ping_inner(port)).await {
		Ok(result) => result,
		Err(_) => Err("no answer in time".to_string()),
	}
}

async fn ping_inner(port: u16) -> Result<PingInfo, String> {
	let mut stream = TcpStream::connect(("127.0.0.1", port))
		.await
		.map_err(|error| format!("could not connect: {error}"))?;

	// Handshake (packet 0x00) followed by the status request (0x00, no data).
	let mut handshake = Vec::new();
	write_varint(&mut handshake, PROTOCOL_ANY);
	write_string(&mut handshake, "127.0.0.1");
	handshake.extend_from_slice(&port.to_be_bytes());
	write_varint(&mut handshake, 1); // next state: status
	let mut out = packet(0x00, &handshake);
	out.extend(packet(0x00, &[]));
	stream.write_all(&out).await.map_err(|error| error.to_string())?;

	// Status response: packet 0x00 holding one string of JSON.
	let (id, data) = read_packet(&mut stream).await?;
	if id != 0x00 {
		return Err(format!("unexpected packet {id:#x}"));
	}
	let json = read_string(&mut data.as_slice())?;

	// Ping (0x01) with 8 bytes; the pong echoes them back.
	let payload: [u8; 8] = 0x4241_5244_4953_434Fu64.to_be_bytes();
	let sent = Instant::now();
	stream.write_all(&packet(0x01, &payload)).await.map_err(|error| error.to_string())?;
	let (id, data) = read_packet(&mut stream).await?;
	let latency_ms = sent.elapsed().as_millis() as u64;
	if id != 0x01 || data != payload {
		return Err("bad pong".to_string());
	}

	parse_status(&json, latency_ms)
}

/// Pull what we show out of the status JSON:
/// {"version":{"name":"1.7.10",...},"players":{"max":20,"online":1,...},"description":...}
fn parse_status(json: &str, latency_ms: u64) -> Result<PingInfo, String> {
	let value: serde_json::Value = serde_json::from_str(json).map_err(|error| format!("bad status JSON: {error}"))?;
	let number = |field: &serde_json::Value| field.as_u64().map(|n| n.min(u32::MAX as u64) as u32);
	Ok(PingInfo {
		latency_ms,
		online: number(&value["players"]["online"]).unwrap_or(0),
		max: number(&value["players"]["max"]).unwrap_or(0),
		version: value["version"]["name"].as_str().unwrap_or("?").chars().take(40).collect(),
		motd: strip_color_codes(&description_text(&value["description"])).chars().take(100).collect(),
	})
}

/// The MOTD is either a plain string or a "chat component":
/// {"text":"A ","extra":[{"text":"Server"}]}. Join all the text parts.
fn description_text(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::String(text) => text.clone(),
		serde_json::Value::Object(map) => {
			let mut text = map.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
			if let Some(extra) = map.get("extra").and_then(|e| e.as_array()) {
				for part in extra {
					text.push_str(&description_text(part));
				}
			}
			text
		}
		_ => String::new(),
	}
}

/// "§aGreen §lbold" -> "Green bold" (§ + one character is a color/style code).
fn strip_color_codes(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let mut chars = text.chars();
	while let Some(c) = chars.next() {
		if c == '§' {
			chars.next();
		} else {
			out.push(c);
		}
	}
	out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------- packets ----------

/// [length][id][data], where length counts id + data.
fn packet(id: i32, data: &[u8]) -> Vec<u8> {
	let mut body = Vec::with_capacity(data.len() + 5);
	write_varint(&mut body, id);
	body.extend_from_slice(data);
	let mut out = Vec::with_capacity(body.len() + 5);
	write_varint(&mut out, body.len() as i32);
	out.extend(body);
	out
}

async fn read_packet(stream: &mut TcpStream) -> Result<(i32, Vec<u8>), String> {
	let length = read_varint_async(stream).await?;
	let length = usize::try_from(length).map_err(|_| "negative packet length".to_string())?;
	if length == 0 || length > MAX_PACKET_LEN {
		return Err(format!("packet length {length} out of range"));
	}
	let mut body = vec![0u8; length];
	stream.read_exact(&mut body).await.map_err(|error| error.to_string())?;
	let mut slice = body.as_slice();
	let id = read_varint(&mut slice)?;
	Ok((id, slice.to_vec()))
}

// ---------- VarInt and strings ----------

fn write_varint(out: &mut Vec<u8>, value: i32) {
	// Work on the raw bits, so negative numbers become 5 bytes (as the protocol says).
	let mut value = value as u32;
	loop {
		let byte = (value & 0x7F) as u8;
		value >>= 7;
		if value == 0 {
			out.push(byte);
			return;
		}
		out.push(byte | 0x80);
	}
}

fn read_varint(input: &mut &[u8]) -> Result<i32, String> {
	let mut value: u32 = 0;
	for i in 0..5 {
		let (&byte, rest) = input.split_first().ok_or("VarInt cut short")?;
		*input = rest;
		value |= ((byte & 0x7F) as u32) << (7 * i);
		if byte & 0x80 == 0 {
			return Ok(value as i32);
		}
	}
	Err("VarInt longer than 5 bytes".to_string())
}

/// Same as `read_varint`, but straight from the socket, one byte at a time.
async fn read_varint_async(stream: &mut TcpStream) -> Result<i32, String> {
	let mut value: u32 = 0;
	for i in 0..5 {
		let byte = stream.read_u8().await.map_err(|error| error.to_string())?;
		value |= ((byte & 0x7F) as u32) << (7 * i);
		if byte & 0x80 == 0 {
			return Ok(value as i32);
		}
	}
	Err("VarInt longer than 5 bytes".to_string())
}

/// A string is [length in bytes: VarInt][UTF-8 bytes].
fn write_string(out: &mut Vec<u8>, text: &str) {
	write_varint(out, text.len() as i32);
	out.extend_from_slice(text.as_bytes());
}

fn read_string(input: &mut &[u8]) -> Result<String, String> {
	let length = usize::try_from(read_varint(input)?).map_err(|_| "negative string length".to_string())?;
	if length > input.len() {
		return Err("string cut short".to_string());
	}
	let (text, rest) = input.split_at(length);
	*input = rest;
	String::from_utf8(text.to_vec()).map_err(|_| "string is not UTF-8".to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn varint_known_values() {
		// From the Minecraft protocol documentation.
		let cases: [(i32, &[u8]); 8] = [
			(0, &[0x00]),
			(1, &[0x01]),
			(127, &[0x7f]),
			(128, &[0x80, 0x01]),
			(255, &[0xff, 0x01]),
			(25565, &[0xdd, 0xc7, 0x01]),
			(2147483647, &[0xff, 0xff, 0xff, 0xff, 0x07]),
			(-1, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
		];
		for (value, bytes) in cases {
			let mut out = Vec::new();
			write_varint(&mut out, value);
			assert_eq!(out, bytes, "write {value}");
			assert_eq!(read_varint(&mut &bytes[..]), Ok(value), "read {value}");
		}
	}

	#[test]
	fn varint_rejects_bad_input() {
		assert!(read_varint(&mut &[][..]).is_err());
		assert!(read_varint(&mut &[0x80][..]).is_err(), "says more follows, but nothing does");
		assert!(read_varint(&mut &[0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..]).is_err(), "too long");
	}

	#[test]
	fn strings_round_trip() {
		let mut out = Vec::new();
		write_string(&mut out, "สวัสดี GTNH");
		assert_eq!(read_string(&mut out.as_slice()), Ok("สวัสดี GTNH".to_string()));
		assert!(read_string(&mut &[0x05, b'a'][..]).is_err(), "length says 5, only 1 byte");
	}

	#[test]
	fn packet_layout() {
		// [length 3][id 0x01][2 data bytes]
		assert_eq!(packet(0x01, &[0xaa, 0xbb]), [0x03, 0x01, 0xaa, 0xbb]);
		assert_eq!(packet(0x00, &[]), [0x01, 0x00]);
	}

	#[test]
	fn parses_a_gtnh_style_status() {
		let json = r#"{"version":{"name":"1.7.10","protocol":5},"players":{"max":20,"online":1,"sample":[{"name":"WAI","id":"x"}]},"description":"§6GT §bNew Horizons"}"#;
		let info = parse_status(json, 3).unwrap();
		assert_eq!(
			info,
			PingInfo { latency_ms: 3, online: 1, max: 20, version: "1.7.10".into(), motd: "GT New Horizons".into() }
		);
	}

	#[test]
	fn motd_as_a_chat_component() {
		let json = r#"{"version":{"name":"1.20"},"players":{"max":5,"online":0},"description":{"text":"Hello ","extra":[{"text":"§aworld"}]}}"#;
		assert_eq!(parse_status(json, 1).unwrap().motd, "Hello world");
	}

	#[test]
	fn rejects_bad_json() {
		assert!(parse_status("not json", 1).is_err());
	}

	/// A tiny fake Minecraft server that answers one status + ping.
	async fn fake_minecraft(status_json: &'static str) -> u16 {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(async move {
			let (mut socket, _) = listener.accept().await.unwrap();
			// Handshake: must say "next state = status" (last VarInt = 1).
			let (id, handshake) = read_packet(&mut socket).await.unwrap();
			assert_eq!(id, 0x00);
			assert_eq!(handshake.last(), Some(&1));
			let (id, request) = read_packet(&mut socket).await.unwrap();
			assert_eq!((id, request.len()), (0x00, 0));

			let mut data = Vec::new();
			write_string(&mut data, status_json);
			socket.write_all(&packet(0x00, &data)).await.unwrap();

			// Echo the ping back as the pong.
			let (id, payload) = read_packet(&mut socket).await.unwrap();
			assert_eq!(id, 0x01);
			socket.write_all(&packet(0x01, &payload)).await.unwrap();
		});
		port
	}

	#[tokio::test]
	async fn pings_a_fake_server() {
		let port = fake_minecraft(r#"{"version":{"name":"1.7.10"},"players":{"max":20,"online":2},"description":"GTNH"}"#).await;
		let info = ping(port).await.unwrap();
		assert_eq!((info.online, info.max, info.version.as_str(), info.motd.as_str()), (2, 20, "1.7.10", "GTNH"));
		assert!(info.latency_ms < 1000);
	}

	#[tokio::test]
	async fn nothing_listening() {
		let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
		assert!(ping(port).await.is_err());
	}

	#[tokio::test]
	async fn a_server_that_never_answers_times_out() {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(async move {
			let _socket = listener.accept().await.unwrap();
			tokio::time::sleep(Duration::from_secs(30)).await;
		});
		let started = Instant::now();
		assert_eq!(ping(port).await, Err("no answer in time".to_string()));
		assert!(started.elapsed() < Duration::from_secs(5));
	}
}
