//! Normal shutdown over a live CSA socket.
//!
//! The server deliberately keeps its write half open after the terminal game
//! sequence. This covers the production failure where the socket reader stayed
//! blocked, kept a log sender alive, and prevented `shell::run()` from joining
//! the log writer.

use std::io::{BufRead as _, Write as _};
use std::time::Duration;

use rsshogi_csa_bridge::config::BridgeConfig;
use rsshogi_csa_bridge::session::SessionCore;
use rsshogi_csa_bridge::shell::{self, ShellConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const GAME_ID: &str = "test+shutdown-10-5+us+them+20260809120000";

const SUMMARY: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+shutdown-10-5+us+them+20260809120000
Name+:us
Name-:them
Your_Turn:+
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:10
Byoyomi:5
Increment:0
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
END Position
END Game_Summary";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_finishes_while_server_keeps_socket_open() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(server_that_keeps_socket_open(listener));

    let log_dir = std::env::temp_dir().join(format!("rsshogi-csa-shutdown-{}", addr.port()));
    let _ = tokio::fs::remove_dir_all(&log_dir).await;

    let mut config = BridgeConfig::floodgate("us", "shutdown-10-5,secret");
    config.games = 1;
    config.margin_ms = 500;

    let engine_path = std::env::current_exe().expect("current test executable");
    let core = SessionCore::new(config, engine_path.display().to_string(), "test");
    let shell_config = ShellConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        engine_path,
        engine_args: vec![
            "--ignored".to_owned(),
            "--exact".to_owned(),
            "fake_usi_engine".to_owned(),
            "--nocapture".to_owned(),
        ],
        engine_dir: None,
        log_dir: log_dir.clone(),
        quit_grace: Duration::from_secs(1),
        redact: vec!["secret".to_owned()],
    };

    let outcome = tokio::time::timeout(Duration::from_secs(5), shell::run(core, shell_config))
        .await
        .expect("shell::run should not wait for server EOF");
    outcome.expect("normal shutdown should succeed");

    let first = read_log_file(&log_dir, "-events.jsonl").await;
    let records: Vec<serde_json::Value> =
        first.lines().map(|line| serde_json::from_str(line).expect("JSON record")).collect();
    assert!(!records.is_empty(), "events log should be flushed");
    assert_eq!(records.last().and_then(|record| record["type"].as_str()), Some("bridge_stop"));
    assert_eq!(
        records.iter().filter(|record| record["type"] == "bridge_stop").count(),
        1,
        "bridge_stop should occur once and be the final record"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = read_log_file(&log_dir, "-events.jsonl").await;
    assert_eq!(first, second, "no event may be appended after bridge_stop");

    let engine_log = read_log_file(&log_dir, "-engine.log").await;
    assert!(engine_log.lines().any(|line| line.contains(">> quit")), "{engine_log}");

    server.abort();
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&log_dir).await;
}

async fn send(write: &mut tokio::net::tcp::OwnedWriteHalf, text: &str) {
    write.write_all(text.as_bytes()).await.expect("write");
    write.write_all(b"\n").await.expect("write newline");
    write.flush().await.expect("flush");
}

async fn server_that_keeps_socket_open(listener: TcpListener) {
    let (stream, _) = listener.accept().await.expect("accept");
    stream.set_nodelay(true).expect("nodelay");
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    let login = lines.next_line().await.expect("read").expect("login line");
    let name = login.split_whitespace().nth(1).unwrap_or("us");
    send(&mut write, &format!("LOGIN:{name} OK")).await;
    for line in SUMMARY.lines() {
        send(&mut write, line).await;
    }

    let agree = next_nonempty(&mut lines).await;
    assert_eq!(agree, format!("AGREE {GAME_ID}"));
    send(&mut write, &format!("START:{GAME_ID}")).await;

    let ours = next_nonempty(&mut lines).await;
    assert!(ours.starts_with('+'), "expected a move, got {ours}");
    let head = ours.split(',').next().unwrap_or(&ours);
    send(&mut write, &format!("{head},T1")).await;
    send(&mut write, "%TORYO").await;
    send(&mut write, "#RESIGN").await;
    send(&mut write, "#WIN").await;

    std::future::pending::<()>().await;
}

async fn next_nonempty(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
) -> String {
    loop {
        let line = lines.next_line().await.expect("read").expect("client line");
        if !line.trim().is_empty() {
            return line;
        }
    }
}

async fn read_log_file(log_dir: &std::path::Path, suffix: &str) -> String {
    let mut entries = tokio::fs::read_dir(log_dir).await.expect("log dir");
    while let Some(entry) = entries.next_entry().await.expect("read log entry") {
        if entry.file_name().to_string_lossy().ends_with(suffix) {
            return tokio::fs::read_to_string(entry.path()).await.expect("read log");
        }
    }
    panic!("{suffix} log not found in {}", log_dir.display());
}

/// Runs as the real child process used by the shutdown regression.
#[test]
#[ignore = "spawned by shutdown_finishes_while_server_keeps_socket_open"]
fn fake_usi_engine() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        match line.expect("engine stdin").as_str() {
            "usi" => {
                writeln!(stdout, "id name shutdown-test-engine").expect("engine stdout");
                writeln!(stdout, "usiok").expect("engine stdout");
            }
            "isready" => writeln!(stdout, "readyok").expect("engine stdout"),
            line if line.starts_with("go ") => {
                writeln!(stdout, "bestmove 7g7f").expect("engine stdout");
            }
            "quit" => break,
            _ => {}
        }
        stdout.flush().expect("flush engine stdout");
    }
}
