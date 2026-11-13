//! Code shared by the two programs in this package:
//! - the Discord bot (`src/main.rs`), which usually runs in Docker
//! - the game launcher (`src/bin/launcher.rs`), which runs on Windows and owns the game servers

pub mod games;
