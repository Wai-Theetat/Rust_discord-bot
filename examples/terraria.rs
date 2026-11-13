// Boot the Terraria server, wait until it's ready, then `exit` it cleanly.
// Same shape as examples/gtnh.rs; only the ready line and stop command differ.
//
// Run with:
//   cargo run --example terraria -- "C:/Program Files (x86)/Steam/steamapps/common/Terraria"

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::oneshot;

const BOOT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const RUN_FOR: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run --example terraria -- <path to Terraria folder>");
        return;
    };

    // Full path to the exe: Windows won't find a bare "TerrariaServer.exe" in current_dir.
    let exe = dir.join("TerrariaServer.exe");
    if !exe.exists() {
        eprintln!("{} not found", exe.display());
        return;
    }

    let mut child = Command::new(&exe)
        .args(["-config", "serverconfig.txt"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start TerrariaServer.exe");

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let (ready_tx, ready_rx) = oneshot::channel::<()>();

    let reader_task = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf);
                    // Terraria redraws progress with '\r' ("Settling liquids 42%"),
                    // so one "line" can hold many updates. Show only the last one.
                    let line = text.trim_end().rsplit('\r').next().unwrap_or("").trim();
                    println!("[server] {line}");

                    if line.contains("Server started") {
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                }
                Err(error) => {
                    eprintln!("read error: {error}");
                    break;
                }
            }
        }
    });

    match tokio::time::timeout(BOOT_TIMEOUT, ready_rx).await {
        Ok(Ok(())) => println!("[me] server is ready. Exiting it in {}s...", RUN_FOR.as_secs()),
        Ok(Err(_)) => {
            println!("[me] server exited before it finished booting (check the log above)");
            let status = child.wait().await.expect("wait failed");
            let _ = reader_task.await;
            println!("exit: {status}");
            return;
        }
        Err(_) => {
            // Usually means it's sitting at a prompt like "Choose World:".
            println!("[me] no \"Server started\" after {}s, killing it", BOOT_TIMEOUT.as_secs());
            let _ = child.kill().await;
            let _ = reader_task.await;
            return;
        }
    }

    tokio::time::sleep(RUN_FOR).await;

    // Terraria's console command for "save the world and shut down".
    send(&mut stdin, "exit").await;

    match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
        Ok(status) => println!("exit: {}", status.expect("wait failed")),
        Err(_) => {
            println!("[me] still running after {}s, force-killing", STOP_TIMEOUT.as_secs());
            let _ = child.kill().await;
        }
    }
    let _ = reader_task.await;
}

async fn send(stdin: &mut ChildStdin, command: &str) {
    println!("[me] > {command}");
    if let Err(error) = stdin.write_all(format!("{command}\r\n").as_bytes()).await {
        eprintln!("could not send {command:?}: {error}");
        return;
    }
    let _ = stdin.flush().await;
}
