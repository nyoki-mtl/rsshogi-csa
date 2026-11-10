//! Real files and a real child: split output loss must not end the current game.
use std::io::{BufRead as _, Write as _};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rsshogi_csa_bridge::{
    shell::{self, ShellConfig},
    BridgeConfig, SessionCore,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const SUMMARY: &str = "BEGIN Game_Summary\nProtocol_Version:1.2\nFormat:Shogi 1.0\nGame_ID:split-test\nName+:us\nName-:them\nYour_Turn:+\nTo_Move:+\nBEGIN Time\nTime_Unit:1sec\nTotal_Time:30\nByoyomi:0\nIncrement:0\nLeast_Time_Per_Move:0\nEND Time\nBEGIN Position\nPI\n+\nEND Position\nEND Game_Summary\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn additional_file_failure_keeps_game_and_legacy_log_but_refuses_next_registration() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let log_dir = std::env::temp_dir()
        .join(format!("rsshogi-csa-split-process-{}-{nonce}", std::process::id()));
    let server_dir = log_dir.clone();
    let server = tokio::spawn(async move {
        let (stream, remote) = listener.accept().await.unwrap();
        assert!(remote.ip().is_loopback());
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        assert!(lines.next_line().await.unwrap().unwrap().starts_with("LOGIN us "));
        write.write_all(b"LOGIN:us OK\n").await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "%%GAME split-test *");
        let mut dirs = tokio::fs::read_dir(&server_dir).await.unwrap();
        let split = loop {
            let entry = dirs.next_entry().await.unwrap().unwrap();
            if entry.file_name().to_string_lossy().ends_with("-split") {
                break entry.path();
            }
        };
        // These are this test's newly created paths, before any game exists.
        // Replace the empty games directory with a regular file so the first
        // game creation fails in the filesystem, not in a synthetic event.
        assert_eq!(split.parent(), Some(server_dir.as_path()));
        tokio::fs::rename(split.join("games"), split.join("retained-games")).await.unwrap();
        tokio::fs::write(split.join("games"), b"block child-file creation").await.unwrap();
        write.write_all(SUMMARY.as_bytes()).await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "AGREE split-test");
        write.write_all(b"START:split-test\n").await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "+7776FU");
        write.write_all(b"+7776FU,T1\n%TORYO\n#RESIGN\n#WIN\n").await.unwrap();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            "LOGOUT",
            "a second registration must not escape"
        );
    });

    let mut config = BridgeConfig::floodgate("us", "secret");
    config.x1 = true;
    config.game_name = Some("split-test".into());
    config.games = 0; // Only the critical failure can stop the next registration.
    let engine_path = std::env::current_exe().unwrap();
    let core = SessionCore::new(config, "child", "test");
    let settings = ShellConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        engine_path,
        engine_args: vec![
            "--ignored".into(),
            "--exact".into(),
            "split_test_engine".into(),
            "--nocapture".into(),
        ],
        engine_dir: None,
        log_dir: log_dir.clone(),
        quit_grace: Duration::from_secs(1),
        redact: vec!["secret".into()],
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let outcome = shell::run(core, settings).await;
        server.await.unwrap();
        outcome
    })
    .await
    .expect("split failure must not stall or drop the current game");
    assert!(outcome.is_err(), "critical output loss must be visible to the operator");
    let mut paths = tokio::fs::read_dir(&log_dir).await.unwrap();
    let mut legacy = None;
    while let Some(entry) = paths.next_entry().await.unwrap() {
        if entry.file_name().to_string_lossy().ends_with("-events.jsonl") {
            assert!(legacy.is_none(), "legacy consumers must see exactly one root run");
            legacy = Some(entry.path());
        }
    }
    let text = tokio::fs::read_to_string(legacy.unwrap()).await.unwrap();
    let records: Vec<serde_json::Value> =
        text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(records.iter().any(|record| record["type"] == "game_end" && record["result"] == "win"));
    assert_eq!(records.iter().filter(|record| record["type"] == "game_start").count(), 1);
    assert_eq!(records.last().unwrap()["type"], "bridge_stop");
    assert_eq!(log_dir.parent(), Some(std::env::temp_dir().as_path()));
    tokio::fs::remove_dir_all(log_dir).await.unwrap();
}

#[test]
#[ignore = "real child spawned by the split-output regression"]
fn split_test_engine() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        match line.unwrap().as_str() {
            "usi" => writeln!(stdout, "usiok").unwrap(),
            "isready" => writeln!(stdout, "readyok").unwrap(),
            line if line.starts_with("go ") => writeln!(stdout, "bestmove 7g7f").unwrap(),
            "quit" => break,
            _ => {}
        }
        stdout.flush().unwrap();
    }
}
