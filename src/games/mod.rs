//! Game servers.
//!
//! The launcher (on Windows) runs servers with `manager` and serves them over HTTP
//! with `launcher`. The bot (in Docker) talks to the launcher with `client`.

pub mod client;
pub mod config;
pub mod events;
pub mod launcher;
pub mod manager;
pub mod mc_ping;
