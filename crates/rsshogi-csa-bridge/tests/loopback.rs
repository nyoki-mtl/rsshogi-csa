//! One whole game over a real socket against a real engine.
//!
//! Everything below the shell is already covered without I/O. This is the one
//! test that proves the plumbing: a TCP listener on loopback speaking CSA, a
//! real USI engine as a child process, and the bridge in between.
//!
//! It needs an engine, so it is opt-in:
//!
//! ```text
//! $env:RSSHOGI_CSA_TEST_ENGINE = "C:\engines\YaneuraOu.exe"
//! cargo test -p rsshogi-csa-bridge --test loopback -- --nocapture
//! ```
//!
//! Without the variable the test reports that it was skipped and passes, so CI
//! stays green on machines with no engine.

use std::path::PathBuf;
use std::time::Duration;

use rsshogi_csa_bridge::config::BridgeConfig;
use rsshogi_csa_bridge::session::SessionCore;
use rsshogi_csa_bridge::shell::{self, ShellConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const GAME_ID: &str = "test+loopback-10-5+us+them+20260804120000";

/// A short control keeps the test quick while still exercising byoyomi.
const SUMMARY: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+loopback-10-5+us+them+20260804120000
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

/// Replies that stay legal whatever the engine opens with.
///
/// A pawn on 3c or 8c can always step forward in the first few plies, and 3b is
/// out of reach of any black opening move, so the gold can always come out.
const REPLIES: [&str; 3] = ["-3334FU", "-8384FU", "-4132KI"];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plays_a_whole_game_over_a_real_socket() {
    play_one_game(false).await;
}

/// The same game with a search running on the opponent's clock.
///
/// The scripted opponent almost never plays what the engine predicts, so this
/// is the miss path against a real engine: `go ponder` and `stop` arrive within
/// microseconds of each other, which is exactly what a server that batches our
/// echo with the reply produces.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plays_a_whole_game_while_pondering() {
    play_one_game(true).await;
}

async fn play_one_game(ponder: bool) {
    let Some(engine_path) = std::env::var_os("RSSHOGI_CSA_TEST_ENGINE") else {
        eprintln!("skipped: set RSSHOGI_CSA_TEST_ENGINE to an engine executable to run this");
        return;
    };
    let engine_path = PathBuf::from(engine_path);
    assert!(engine_path.is_file(), "{} is not a file", engine_path.display());

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(fake_server(listener));

    let log_dir = std::env::temp_dir().join(format!("rsshogi-csa-loopback-{}", addr.port()));
    let _ = tokio::fs::remove_dir_all(&log_dir).await;

    let mut config = BridgeConfig::floodgate("us", "loopback-10-5,secret");
    config.games = 1;
    config.margin_ms = 500;
    config.engine_options = vec![
        // The only switch pondering has: the bridge drives it whenever the
        // engine offers a move to ponder on.
        ("USI_Ponder".to_owned(), ponder.to_string()),
        ("Threads".to_owned(), "1".to_owned()),
        ("USI_Hash".to_owned(), "64".to_owned()),
        // Keep the engine from spending its full budget on every move.
        ("MinimumThinkingTime".to_owned(), "1".to_owned()),
        ("NetworkDelay".to_owned(), "0".to_owned()),
        ("NetworkDelay2".to_owned(), "0".to_owned()),
    ];

    let core = SessionCore::new(config, engine_path.display().to_string(), "test");
    let shell_config = ShellConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        engine_path,
        engine_args: Vec::new(),
        engine_dir: None,
        log_dir: log_dir.clone(),
        quit_grace: Duration::from_secs(6),
        redact: vec!["secret".to_owned()],
    };

    let outcome = tokio::time::timeout(Duration::from_secs(150), shell::run(core, shell_config))
        .await
        .expect("the bridge should finish well inside the timeout");
    outcome.expect("the bridge should not fail with an io error");

    let transcript = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake server should finish")
        .expect("the fake server task should not panic");

    // The handshake happened in the right order.
    assert!(transcript[0].starts_with("LOGIN us "), "{transcript:?}");
    assert_eq!(transcript[1], format!("AGREE {GAME_ID}"), "{transcript:?}");

    // Every move we sent was a syntactically valid black move, and there were
    // as many as the opponent replied to.
    let moves: Vec<&String> = transcript[2..].iter().filter(|line| line.starts_with('+')).collect();
    assert_eq!(moves.len(), REPLIES.len(), "one move per reply: {transcript:?}");
    for line in &moves {
        let head = line.split(',').next().unwrap_or_default();
        assert!(
            rsshogi_csa::CsaMove::parse(head).is_some(),
            "`{line}` is not a legal-looking move line"
        );
    }

    // The floodgate comment is what the server records and shows, so its shape
    // has to survive a real engine's variation and not just a fixture's. How
    // long that variation is depends on how deep the engine got, which under
    // load is not something to assert on.
    let commented: Vec<&&String> = moves.iter().filter(|line| line.contains(",'* ")).collect();
    assert!(!commented.is_empty(), "no move carried a score comment: {moves:?}");
    for line in &commented {
        let (_, rest) = line.split_once(",'* ").expect("filtered on this");
        let mut fields = rest.split(' ');
        fields.next().expect("the score").parse::<i32>().expect("the score is a number");
        for pv_move in fields {
            assert!(
                rsshogi_csa::CsaMove::parse(pv_move).is_some(),
                "`{pv_move}` in `{line}` is not a move"
            );
        }
    }

    // The event log exists and records the game from start to finish.
    let events = read_events(&log_dir).await;
    assert!(events.iter().any(|line| line.contains(r#""type":"game_start""#)), "no game_start");
    assert!(events.iter().any(|line| line.contains(r#""type":"go""#)), "no go");
    assert!(events.iter().any(|line| line.contains(r#""type":"move""#)), "no move");
    assert!(events.iter().any(|line| line.contains(r#""type":"game_end""#)), "no game_end");
    assert!(events.iter().all(|line| line.contains(r#""schema":1"#)), "envelope missing");
    assert_eq!(
        events.iter().any(|line| line.contains(r#""type":"ponder""#)),
        ponder,
        "the opponent's clock was used exactly when it was asked for"
    );
    if ponder {
        assert!(
            events.iter().any(|line| line.contains(r#""outcome":"started""#)),
            "a search should have run on the opponent's clock"
        );
        assert!(
            events.iter().any(|line| {
                line.contains(r#""outcome":"hit""#) || line.contains(r#""outcome":"miss""#)
            }),
            "every ponder has to resolve one way or the other"
        );
    }

    // Nothing went wrong along the way.
    let alerts: Vec<&String> =
        events.iter().filter(|line| line.contains(r#""type":"alert""#)).collect();
    assert!(alerts.is_empty(), "the run produced alerts: {alerts:#?}");

    let _ = tokio::fs::remove_dir_all(&log_dir).await;
}

async fn send(write: &mut tokio::net::tcp::OwnedWriteHalf, text: &str) {
    write.write_all(text.as_bytes()).await.expect("write");
    write.write_all(b"\n").await.expect("write newline");
    write.flush().await.expect("flush");
}

/// Speaks just enough CSA to run one game, and returns what the client sent.
async fn fake_server(listener: TcpListener) -> Vec<String> {
    let (stream, _) = listener.accept().await.expect("accept");
    stream.set_nodelay(true).expect("nodelay");
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut received = Vec::new();

    // LOGIN
    let login = lines.next_line().await.expect("read").expect("login line");
    received.push(login.clone());
    let name = login.split_whitespace().nth(1).unwrap_or("us").to_owned();
    send(&mut write, &format!("LOGIN:{name} OK")).await;

    // Propose the game.
    for line in SUMMARY.lines() {
        send(&mut write, line).await;
    }

    // AGREE, then START.
    let agree = loop {
        let line = lines.next_line().await.expect("read").expect("agree line");
        if line.trim().is_empty() {
            continue; // a keepalive, which is legal at any time
        }
        break line;
    };
    received.push(agree);
    send(&mut write, &format!("START:{GAME_ID}")).await;

    // Play the scripted opponent.
    for reply in REPLIES {
        let ours = loop {
            let line = lines.next_line().await.expect("read").expect("a move from the client");
            if line.trim().is_empty() {
                continue;
            }
            break line;
        };
        received.push(ours.clone());
        if ours.starts_with('%') {
            break; // the client resigned or declared; stop scripting
        }
        // The server strips the client's comment before broadcasting, and adds
        // the time it charged.
        let head = ours.split(',').next().unwrap_or(&ours).to_owned();
        send(&mut write, &format!("{head},T1")).await;
        send(&mut write, &format!("{reply},T1")).await;
    }

    // End the game with the client winning, as a real burst would arrive.
    send(&mut write, "%TORYO").await;
    send(&mut write, "#RESIGN").await;
    send(&mut write, "#WIN").await;
    drop(write);
    received
}

async fn read_events(log_dir: &std::path::Path) -> Vec<String> {
    let mut entries = tokio::fs::read_dir(log_dir).await.expect("log dir should exist");
    let mut events = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().ends_with("-events.jsonl") {
            let text = tokio::fs::read_to_string(entry.path()).await.expect("read events");
            events = text.lines().map(str::to_owned).collect();
        }
    }
    assert!(!events.is_empty(), "the event log should not be empty");
    events
}
