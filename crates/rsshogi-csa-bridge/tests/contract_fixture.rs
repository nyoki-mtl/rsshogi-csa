//! The canonical event-log fixture, and the test that keeps it honest.
//!
//! The event log is a contract with a reader outside this workspace, so both
//! sides have to agree on one artifact rather than on two descriptions of it.
//! This test drives the real `SessionCore` through a run and writes the bytes a
//! bridge would write; `ShogiArena` vendors the result verbatim and parses it with
//! its real reader.
//!
//! **This file is the producer of record.** The copy in `ShogiArena` is a vendored
//! artifact — it is never edited there, and its provenance file names the commit
//! and SHA-256 it came from. Neither repository's CI reads a sibling checkout:
//! the bytes travel by being committed, which is the only way a cross-repository
//! fixture stays reproducible.
//!
//! Regenerate after an intentional schema change:
//!
//! ```text
//! cargo test -p rsshogi-csa-bridge --test contract_fixture -- --ignored
//! ```
//!
//! then copy the file into `ShogiArena` and update its provenance.

use std::path::PathBuf;

use rsshogi_csa_bridge::{Action, BridgeConfig, Event, LogRecord, SessionCore};

/// A run that exercises every distinction the contract makes.
const PROPOSAL_DECLARED: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+floodgate-300-10F+us+them+20260807120000
Name+:us
Name-:them
Your_Turn:+
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:300
Byoyomi:0
Increment:10
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
END Position
END Game_Summary";

const PROPOSAL_WHITE: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+floodgate-300-10F+them2+us+20260807123000
Name+:them2
Name-:us
Your_Turn:-
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:300
Byoyomi:0
Increment:10
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
END Position
END Game_Summary";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/csa-event-log-contract.jsonl")
}

/// Serialise exactly as `writer_task` does: the envelope gains `schema` and a
/// wall-clock `ts`. The timestamp is derived from `at` so the artifact is
/// byte-stable across runs — a fixture that changed every time it was generated
/// could not be compared.
fn to_wire(record: &LogRecord) -> String {
    const EPOCH: u64 = 1_800_000_000_000;
    let mut value = serde_json::to_value(record).expect("a record serialises");
    let object = value.as_object_mut().expect("a record is an object");
    object.insert("schema".to_owned(), serde_json::json!(1));
    object.insert("ts".to_owned(), serde_json::json!(EPOCH + record.at));
    serde_json::to_string(&value).expect("a record serialises")
}

fn config() -> BridgeConfig {
    let mut config = BridgeConfig::floodgate("us", "floodgate-300-10F,secret");
    config.games = 2;
    // Run-static options: applied once at initialisation, true for the whole run.
    config.engine_options =
        vec![("Threads".to_owned(), "4".to_owned()), ("USI_Hash".to_owned(), "1024".to_owned())];
    config
}

fn feed(core: &mut SessionCore, event: Event, lines: &mut Vec<String>) {
    for action in core.step(event) {
        if let Action::Log(record) = action {
            lines.push(to_wire(&record));
        }
    }
}

fn start_run(core: &mut SessionCore, lines: &mut Vec<String>) {
    feed(core, Event::Started { at: 0 }, lines);
    feed(core, Event::EngineLine { at: 5, line: "id name Fixture Engine".to_owned() }, lines);
    feed(core, Event::EngineLine { at: 6, line: "id author Fixture Author".to_owned() }, lines);
    feed(core, Event::EngineLine { at: 10, line: "usiok".to_owned() }, lines);
    feed(core, Event::EngineLine { at: 20, line: "readyok".to_owned() }, lines);
    feed(core, Event::EngineLine { at: 25, line: "not a usi command".to_owned() }, lines);
    feed(core, Event::Connected { at: 30 }, lines);
    feed(core, Event::ServerLine { at: 40, line: "LOGIN:us OK".to_owned() }, lines);

    // A quiet stretch, so the fixture carries a heartbeat.
    feed(core, Event::Tick { at: 40 + 60_000 }, lines);
}

fn play_first_game(core: &mut SessionCore, lines: &mut Vec<String>) {
    let base = 100_000;
    for (index, line) in PROPOSAL_DECLARED.lines().enumerate() {
        let at = base + index as u64;
        feed(core, Event::ServerLine { at, line: line.to_owned() }, lines);
    }
    feed(
        core,
        Event::ServerLine {
            at: base + 1_000,
            line: "START:test+floodgate-300-10F+us+them+20260807120000".to_owned(),
        },
        lines,
    );

    feed(core, Event::EngineLine { at: base + 2_000, line: "bestmove 7g7f".to_owned() }, lines);
    feed(core, Event::ServerLine { at: base + 2_100, line: "+7776FU,T2".to_owned() }, lines);
    feed(core, Event::ServerLine { at: base + 3_000, line: "-3334FU,T3".to_owned() }, lines);
    feed(core, Event::EngineLine { at: base + 4_000, line: "bestmove resign".to_owned() }, lines);
    feed(core, Event::ServerLine { at: base + 4_100, line: "%TORYO".to_owned() }, lines);
    feed(core, Event::ServerLine { at: base + 4_200, line: "#RESIGN".to_owned() }, lines);
    feed(core, Event::ServerLine { at: base + 4_300, line: "#LOSE".to_owned() }, lines);

    // floodgate closes the socket after every game. The run remains alive,
    // reconnects, and receives a second pairing with the opposite colour.
    feed(core, Event::ServerEof { at: base + 5_000 }, lines);
    feed(core, Event::Connected { at: base + 5_100 }, lines);
    feed(core, Event::ServerLine { at: base + 5_200, line: "LOGIN:us OK".to_owned() }, lines);
}

fn play_second_game(core: &mut SessionCore, lines: &mut Vec<String>) {
    let base = 100_000;
    let second_base = base + 10_000;
    for (index, line) in PROPOSAL_WHITE.lines().enumerate() {
        let at = second_base + index as u64;
        feed(core, Event::ServerLine { at, line: line.to_owned() }, lines);
    }
    feed(
        core,
        Event::ServerLine {
            at: second_base + 1_000,
            line: "START:test+floodgate-300-10F+them2+us+20260807123000".to_owned(),
        },
        lines,
    );
    feed(core, Event::ServerLine { at: second_base + 2_000, line: "+7776FU,T2".to_owned() }, lines);
    feed(
        core,
        Event::EngineLine { at: second_base + 3_000, line: "bestmove 3c3d".to_owned() },
        lines,
    );
    feed(core, Event::ServerLine { at: second_base + 3_100, line: "-3334FU,T3".to_owned() }, lines);
    feed(core, Event::ServerLine { at: second_base + 4_000, line: "%TORYO".to_owned() }, lines);
    feed(core, Event::ServerLine { at: second_base + 4_100, line: "#RESIGN".to_owned() }, lines);
    feed(
        core,
        Event::EngineLine { at: second_base + 1_500, line: "still not usi".to_owned() },
        lines,
    );
    feed(core, Event::ServerLine { at: second_base + 4_200, line: "#WIN".to_owned() }, lines);
}

/// Drive a whole run and collect what it wrote.
fn produce() -> Vec<String> {
    let mut core = SessionCore::new(config(), "engine.exe", "0.1.0");
    let mut lines = Vec::new();
    start_run(&mut core, &mut lines);
    play_first_game(&mut core, &mut lines);
    play_second_game(&mut core, &mut lines);

    lines
}

/// The artifact must be what the producer actually produces.
///
/// Both repositories read the committed file; this is what stops it drifting
/// away from the code that is supposed to have written it.
#[test]
fn the_committed_fixture_is_what_the_bridge_writes() {
    let produced = produce().join("\n") + "\n";
    let path = fixture_path();
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("missing contract fixture {}: {err}", path.display()));
    assert_eq!(
        produced, committed,
        "the event log drifted from the committed contract fixture; regenerate it with \
         `cargo test -p rsshogi-csa-bridge --test contract_fixture -- --ignored` and re-vendor \
         it into ShogiArena with a fresh provenance record",
    );
}

/// The distinctions the contract turns on, asserted on the producer side too.
///
/// `ShogiArena` asserts the same facts from the same bytes. Two independent
/// readings of one artifact is the point — a description agreed in prose is what
/// drifted last time.
#[test]
fn the_fixture_separates_run_static_options_from_per_game_rules() {
    let lines = produce();
    let records: Vec<serde_json::Value> =
        lines.iter().map(|line| serde_json::from_str(line).expect("valid json")).collect();

    let meta = records
        .iter()
        .find(|record| record["type"] == "engine_meta")
        .expect("the run states what the engine is");
    let names: Vec<&str> = meta["options"]
        .as_array()
        .expect("options is a list")
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Threads", "USI_Hash"], "engine_meta carries the run-static options");
    assert!(
        !names.contains(&"EnteringKingRule"),
        "the entering-king rule is decided per game, so it is not part of the engine's identity",
    );

    let start = records
        .iter()
        .find(|record| record["type"] == "game_start")
        .expect("the run starts a game");
    assert!(
        start["entering_king_rule"].is_string(),
        "game_start is the single source of truth for the rule this game was played under",
    );

    assert!(
        records.iter().any(|record| record["type"] == "heartbeat"),
        "a quiet stretch is filled"
    );
    assert!(records.iter().any(|record| record["type"] == "game_end"));

    let starts: Vec<&serde_json::Value> =
        records.iter().filter(|record| record["type"] == "game_start").collect();
    assert_eq!(starts.len(), 2, "the contract covers a run with two games");
    assert_eq!(starts[0]["my_color"], "black");
    assert_eq!(starts[1]["my_color"], "white");

    let first_end = records
        .iter()
        .position(|record| record["type"] == "game_end")
        .expect("the first game ends");
    let second_start = records
        .iter()
        .rposition(|record| record["type"] == "game_start")
        .expect("the second game starts");
    assert!(
        records[first_end + 1..second_start]
            .iter()
            .any(|record| record["type"] == "state" && record["phase"] == "idle"),
        "idle between games is run liveness, not run completion",
    );
    assert_eq!(
        records.last().map(|record| &record["type"]),
        Some(&serde_json::json!("bridge_stop"))
    );

    let alerts: Vec<&serde_json::Value> =
        records.iter().filter(|record| record["type"] == "alert").collect();
    assert!(
        alerts.iter().any(|record| record["game_id"].is_null()),
        "run-level alerts keep a null game identity",
    );
    assert!(
        alerts.iter().any(|record| record["game_id"].is_string()),
        "game-scoped alerts keep their game identity",
    );
}

/// Regeneration, run on purpose.
#[test]
#[ignore = "writes the fixture; run deliberately after an intentional schema change"]
fn regenerate_the_contract_fixture() {
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().expect("fixture has a parent"))
        .expect("fixture directory");
    std::fs::write(&path, produce().join("\n") + "\n").expect("write fixture");
    eprintln!("wrote {}", path.display());
}
