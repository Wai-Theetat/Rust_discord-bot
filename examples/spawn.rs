use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[tokio::main]
async fn main() {
    // 1. Start another program. Like typing `ping -n 5 127.0.0.1` in a terminal.
    let mut child = Command::new("ping")
        .args(["-n", "5", "127.0.0.1"])
        // Without this, ping prints straight to your screen and Rust can't see it.
        // `piped()` makes a "pipe" so the output comes to us instead.
        .stdout(Stdio::piped())
        .spawn() // <- the program starts running HERE
        .expect("failed to start ping");

    // 2. Grab the output pipe. `take()` moves it out of `child` so we own it.
    let stdout = child.stdout.take().expect("stdout was piped");
    // BufReader lets us read line by line instead of byte by byte.
    let mut reader = BufReader::new(stdout);

    // 3. Read lines until the program closes its output.
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Waits until a whole line ('\n') has arrived. Returns how many bytes were read.
        let bytes_read = reader.read_until(b'\n', &mut buf).await.expect("read failed");
        if bytes_read == 0 {
            break; // 0 bytes = the program finished and closed its output
        }
        // Bytes -> text. "lossy" means any weird bytes become a replacement character instead of crashing.
        let line = String::from_utf8_lossy(&buf);
        println!("[server] {}", line.trim_end()); // trim_end removes the "\r\n"
    }

    // 4. Wait for the program to fully exit, and get its exit code.
    let status = child.wait().await.expect("wait failed");
    println!("exit: {status}");
}