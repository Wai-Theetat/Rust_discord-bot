// Step C: boot a real GTNH server, wait until it's ready, then stop it cleanly.
//
// Run with:
//   cargo run --example gtnh -- "D:/path/to/your/gtnh/server"
//
// If something goes wrong and Java keeps running, end `java.exe` in Task Manager.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::oneshot;

/// GTNH can take several minutes to boot. Give up after this long.
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long to keep the server running after it's ready, before sending `stop`.
const RUN_FOR: Duration = Duration::from_secs(30);
/// How long `stop` may take (saving worlds) before we force-kill.
const STOP_TIMEOUT: Duration = Duration::from_secs(180);

#[tokio::main]
async fn main() {
    // 1. The server folder comes from the command line (`-- "D:/..."`).
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run --example gtnh -- <path to GTNH server folder>");
        return;
    };
    if let Err(problem) = preflight(&dir) {
        eprintln!("{problem}");
        return;
    }

    // 2. Same command as your startserver-java9.bat, minus `pause`.
    //    `current_dir` = run it inside the server folder, like double-clicking the bat.
    let mut child = Command::new("java")
        .args([
            "-Xms6G",
            "-Xmx6G",
            "-Dfml.readTimeout=180",
            "@java9args.txt",
            "-jar",
            "lwjgl3ify-forgePatches.jar",
            "nogui",
        ])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start java (is it installed and on PATH?)");

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut stdin = child.stdin.take().expect("stdin was piped");

    // 3. A one-time message from the reader task to main: "the server is ready".
    let (ready_tx, ready_rx) = oneshot::channel::<()>();

    let reader_task = tokio::spawn(async move {
        // `send()` consumes the sender, so it can only be used once.
        // Keep it in an Option and `take()` it the first time we see "Done (".
        let mut ready_tx = Some(ready_tx);

        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end();
                    println!("[server] {line}");

                    // Minecraft prints: Done (95.123s)! For help, type "help"
                    if line.contains("Done (") && line.contains("For help") {
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(()); // Err only if main stopped listening
                        }
                    }
                }
                Err(error) => {
                    eprintln!("read error: {error}");
                    break;
                }
            }
        }
        // When this task ends, `ready_tx` is dropped. If the server never got
        // ready, main's `ready_rx` then gets an error instead of hanging forever.
    });

    // 4. Wait for "ready", but not forever.
    match tokio::time::timeout(BOOT_TIMEOUT, ready_rx).await {
        Ok(Ok(())) => {
            println!("[me] server is ready. Stopping it in {}s...", RUN_FOR.as_secs());
        }
        Ok(Err(_)) => {
            // The reader ended without seeing "Done (": the process exited during boot.
            println!("[me] server exited before it finished booting (check the log above)");
            let status = child.wait().await.expect("wait failed");
            let _ = reader_task.await;
            println!("exit: {status}");
            return;
        }
        Err(_) => {
            println!("[me] no \"Done (\" after {}s, killing it", BOOT_TIMEOUT.as_secs());
            let _ = child.kill().await;
            let _ = reader_task.await;
            return;
        }
    }

    tokio::time::sleep(RUN_FOR).await;

    // 5. Clean shutdown: this is exactly what `/game stop` will do.
    send(&mut stdin, "stop").await;

    match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
        Ok(status) => println!("exit: {}", status.expect("wait failed")),
        Err(_) => {
            println!("[me] still running after {}s, force-killing", STOP_TIMEOUT.as_secs());
            let _ = child.kill().await;
        }
    }
    let _ = reader_task.await;
}

/// Catch the common "Java exits instantly" causes before starting.
fn preflight(dir: &Path) -> Result<(), String> {
    for file in ["lwjgl3ify-forgePatches.jar", "java9args.txt"] {
        if !dir.join(file).exists() {
            return Err(format!("{file} not found in {}", dir.display()));
        }
    }

    let eula = std::fs::read_to_string(dir.join("eula.txt")).unwrap_or_default();
    if !eula.lines().any(|line| line.trim() == "eula=true") {
        return Err(format!(
            "EULA not accepted: set eula=true in {}",
            dir.join("eula.txt").display()
        ));
    }

    Ok(())
}

/// Write one line to the server console, as if typed and Enter pressed.
async fn send(stdin: &mut ChildStdin, command: &str) {
    println!("[me] > {command}");
    let line = format!("{command}\r\n");
    if let Err(error) = stdin.write_all(line.as_bytes()).await {
        eprintln!("could not send {command:?}: {error}");
        return;
    }
    let _ = stdin.flush().await;
}
