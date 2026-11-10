//! Real unread child pipes must not stop CSA terminal processing or core deadlines.
use std::io::Write as _;
use std::time::{Duration, Instant};

use rsshogi_csa_bridge::shell::{self, ShellConfig};
use rsshogi_csa_bridge::{BridgeConfig, SessionCore};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const SUMMARY: &str = "BEGIN Game_Summary\nProtocol_Version:1.2\nProtocol_Mode:Server\nFormat:Shogi 1.0\nDeclaration:Jishogi 1.1\nGame_ID:blocked-pipe\nName+:us\nName-:them\nYour_Turn:+\nRematch_On_Draw:NO\nTo_Move:+\nMax_Moves:256\nBEGIN Time\nTime_Unit:1sec\nTotal_Time:1\nByoyomi:1\nIncrement:0\nLeast_Time_Per_Move:0\nEND Time\nBEGIN Position\nPI\n+\nEND Position\nEND Game_Summary\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_child_pipe_allows_fallback_terminal_and_bounded_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let addr = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        assert!(lines.next_line().await.unwrap().unwrap().starts_with("LOGIN us "));
        write.write_all(b"LOGIN:us OK\n").await.unwrap();
        write.write_all(SUMMARY.as_bytes()).await.unwrap();
        assert_eq!(next_nonempty(&mut lines).await, "AGREE blocked-pipe");
        write.write_all(b"START:blocked-pipe\n").await.unwrap();
        // No engine bestmove can arrive. Only the live core timer can produce
        // this fallback while its writer is stuck in an actual OS child pipe.
        assert_eq!(next_nonempty(&mut lines).await, "%TORYO");
        write.write_all(b"%TORYO\n#RESIGN\n#LOSE\n").await.unwrap();
        assert_eq!(next_nonempty(&mut lines).await, "LOGOUT");
        std::future::pending::<()>().await;
    });
    let log_dir = std::env::temp_dir().join(format!("rsshogi-csa-unread-pipe-{}", addr.port()));
    let mut config = BridgeConfig::floodgate("us", "pipe-1-1,secret");
    config.games = 1;
    config.margin_ms = 1500;
    config.stop_grace_ms = 1000;
    // Far larger than an OS pipe. The child deliberately never reads stdin.
    config.engine_options = vec![("BlockPipe".into(), "x".repeat(8 * 1024 * 1024))];
    let engine_path = std::env::current_exe().expect("test executable");
    let core = SessionCore::new(config, engine_path.display().to_string(), "test");
    let shell_config = ShellConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        engine_path,
        engine_args: vec![
            "--ignored".into(),
            "--exact".into(),
            "unread_stdin_engine".into(),
            "--nocapture".into(),
        ],
        engine_dir: None,
        log_dir: log_dir.clone(),
        quit_grace: Duration::from_millis(150),
        redact: vec!["secret".into()],
    };
    let start = Instant::now();
    tokio::time::timeout(Duration::from_secs(8), shell::run(core, shell_config))
        .await
        .expect("pipe cannot stall run")
        .expect("recorded shutdown");
    assert!(start.elapsed() < Duration::from_secs(7));
    let mut files = tokio::fs::read_dir(&log_dir).await.unwrap();
    let mut events = String::new();
    let mut engine = String::new();
    while let Some(file) = files.next_entry().await.unwrap() {
        let name = file.file_name().to_string_lossy().to_string();
        if name.ends_with("-events.jsonl") {
            events = tokio::fs::read_to_string(file.path()).await.unwrap();
        }
        if name.ends_with("-engine.log") {
            engine = tokio::fs::read_to_string(file.path()).await.unwrap();
        }
    }
    let records: Vec<serde_json::Value> =
        events.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.last().unwrap()["type"], "bridge_stop");
    assert!(engine.contains("engine exceeded quit grace; killed; exit status:"), "{engine}");
    assert!(
        !engine.contains(">> setoption name BlockPipe"),
        "unfinished write must not be logged as completed"
    );
    server.abort();
    let _ = server.await;
    tokio::fs::remove_dir_all(log_dir).await.unwrap();
}

async fn next_nonempty(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
) -> String {
    loop {
        let line = lines.next_line().await.unwrap().expect("client line");
        if !line.is_empty() {
            return line;
        }
    }
}

/// This is intentionally a hostile I/O peer: it advertises readiness up front
/// to let the shell exercise the Playing path, then leaves stdin unread forever.
#[test]
#[ignore = "real child for unread pipe regression"]
fn unread_stdin_engine() {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "usiok\nreadyok").unwrap();
    stdout.flush().unwrap();
    loop {
        std::thread::park();
    }
}
