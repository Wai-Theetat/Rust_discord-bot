// Step B: read a program's output AND type into it at the same time.
//
// We start `cmd` (the Windows command prompt). It reads commands from its
// stdin, exactly like a Minecraft/Terraria server console reads `stop`/`exit`.
//
// Run with: cargo run --example talk

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::main]
async fn main() {
    // 1. Start cmd with BOTH pipes: stdout (we read) and stdin (we write).
    let mut child = Command::new("cmd")
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start cmd");

    // 2. Take both pipes out of `child`. Each one gets its own owner:
    //    - stdout goes to the helper task (reads)
    //    - stdin stays here in main (writes)
    //    - `child` itself stays in main for `wait()`
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut stdin = child.stdin.take().expect("stdin was piped");

    // 3. Hand the read loop to a helper task (tokio::spawn).
    //    `async move` = the task OWNS `stdout`, because it may keep running
    //    while main moves on.
    let reader_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break, // program closed its output (it exited)
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    println!("[cmd] {}", line.trim_end());
                }
                Err(error) => {
                    eprintln!("read error: {error}");
                    break;
                }
            }
        }
        println!("[reader] output closed");
    });

    // 4. main is free right away. Give cmd a moment to print its banner.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 5. Type commands. "\r\n" = pressing Enter on Windows.
    send(&mut stdin, "echo hello from rust").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    send(&mut stdin, "echo the time is %TIME%").await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // This is what `/game stop` will do later: send the game's stop command.
    send(&mut stdin, "exit").await;

    // 6. Wait for both: the program to exit, and the helper to finish reading.
    let status = child.wait().await.expect("wait failed");
    reader_task.await.expect("reader task panicked");
    println!("exit: {status}");
}

/// Write one line to the program, as if typed and Enter pressed.
async fn send(stdin: &mut tokio::process::ChildStdin, command: &str) {
    println!("[me] > {command}");
    stdin
        .write_all(format!("{command}\r\n").as_bytes())
        .await
        .expect("write failed");
    // Make sure it's actually sent now, not sitting in a buffer.
    stdin.flush().await.expect("flush failed");
}
