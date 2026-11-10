//! The event log the session emits.
//!
//! These records are two things at once: the data source for a viewer, and the
//! forensic trail for a tournament. So the schema evolves by addition only —
//! never change what an existing field means, never remove one.
//!
//! The raw wire log is a separate concern owned by the shell. It has to capture
//! bytes that failed to parse, which by definition never become records.
//!
//! Every type here round-trips. The reader lives outside this workspace now, so
//! `wire_lines_round_trip_byte_for_byte` in this file is the only mechanical
//! guard the contract has left: it pins real log lines and fails the moment a
//! field is renamed, retyped, or dropped. Do not delete it, and do not drop the
//! `Deserialize` derives — "this shape reads back" is itself part of the
//! contract, not a convenience for a particular consumer.
//!
//! That is also why the string-valued fields are enums rather than
//! `&'static str` — the wire spelling is unchanged, but a reader can name it.
//! `entering_king_rule` is the exception and stays a `String`: its values come
//! from the proposal, are not `snake_case`, and may grow.

use serde::{Deserialize, Serialize};

use crate::clock::UsiGoTimes;

/// Monotonic milliseconds, supplied by the shell. The core never reads a clock.
pub type Millis = u64;

/// Which side of the board something belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Sente.
    Black,
    /// Gote.
    White,
}

/// How a game ended, from our side of the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameResult {
    /// We won.
    Win,
    /// We lost.
    Lose,
    /// A draw, including a censored game.
    Draw,
    /// The terminal burst did not say.
    Unknown,
}

/// How loud an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    /// Something to look at after the game.
    Warn,
    /// Something that is costing us the game right now.
    Error,
}

/// What went wrong. Codes are stable; the detail string is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertCode {
    /// The engine produced no `bestmove` before the hard deadline.
    EngineHang,
    /// The engine answered inside the stop grace rather than before the deadline.
    EngineSlow,
    /// The engine's move failed the legality gate.
    IllegalBestmove,
    /// `bestmove win` was sent on, but our own check of the summary rule
    /// disagreed with it.
    UnverifiedDeclaration,
    /// `bestmove resign` was passed on, but the score did not explain it.
    SuspectResign,
    /// A `bestmove` arrived for a search we had already abandoned.
    StaleBestmove,
    /// The engine process died.
    EngineDead,
    /// A replacement engine is being started mid-game.
    EngineRestarted,
    /// A line from the server could not be parsed.
    UnreadableServerLine,
    /// A line from the engine could not be parsed.
    UnreadableEngineLine,
    /// A move from the server was rejected by our own board.
    UnexpectedServerMove,
    /// The connection dropped. On a CSA server this loses the game.
    ConnectionLost,
    /// Login was refused.
    LoginFailed,
    /// The proposal could not be turned into a playable game.
    UnusableProposal,
    /// The configured safety margin leaves no time for the engine to search.
    MarginExhaustsBudget,
    /// The move the engine offered to ponder on failed the legality gate.
    UnusablePonder,
}

/// Which side of the board a name belongs to, for the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerNames {
    /// `Name+`
    pub black: String,
    /// `Name-`
    pub white: String,
}

/// The time control, flattened into milliseconds for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSummary {
    /// `Total_Time` in milliseconds.
    pub total_ms: u64,
    /// `Byoyomi` in milliseconds.
    pub byoyomi_ms: u64,
    /// `Increment` in milliseconds.
    pub inc_ms: u64,
    /// One `Time_Unit` in milliseconds.
    pub unit_ms: u64,
    /// `Least_Time_Per_Move` in milliseconds.
    pub least_ms: u64,
    /// `Time_Roundup`.
    pub roundup: bool,
}

/// A move as the log records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRecord {
    /// 1-based move number.
    pub ply: u32,
    /// Which side played it.
    pub side: Side,
    /// USI spelling, which is what a board renderer wants.
    pub usi: String,
    /// CSA spelling, which is what the wire carried.
    pub csa: String,
    /// What the server charged, in milliseconds.
    pub t_ms: u64,
    /// Black's remaining main time after the move.
    pub black_remaining_ms: u64,
    /// White's remaining main time after the move.
    pub white_remaining_ms: u64,
    /// Our engine's evaluation, when the move was ours.
    pub eval: Option<EvalRecord>,
    /// How the move was chosen.
    pub by: MoveOrigin,
}

/// Where a move came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveOrigin {
    /// The opponent played it.
    Opponent,
    /// It arrived already played, inside the proposal's position block.
    Replayed,
    /// The engine chose it.
    Engine,
    /// The engine failed, so the last principal variation was used.
    FallbackPv,
    /// The engine failed and no principal variation was available.
    FallbackAnyLegal,
}

/// What became of a search run on the opponent's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PonderOutcome {
    /// A search started on a position the opponent has not reached yet.
    Started,
    /// The opponent played the predicted move. The same search continues.
    Hit,
    /// The opponent played something else. The search is thrown away.
    Miss,
}

/// What the engine thought.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRecord {
    /// Centipawns, when the score was not a mate score.
    pub cp: Option<i32>,
    /// Distance to mate in plies, signed.
    pub mate: Option<i32>,
    /// Search depth.
    pub depth: Option<u32>,
    /// Nodes searched.
    pub nodes: Option<u64>,
    /// Principal variation in USI spelling.
    pub pv_usi: Vec<String>,
}

/// The phase the session is in, for a viewer's status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Bringing the engine up. Nothing is connected yet.
    Warming,
    /// Opening the socket and logging in.
    Connecting,
    /// Logged in, waiting to be paired.
    WaitingPairing,
    /// A proposal was accepted, waiting for `START`.
    Agreed,
    /// A game is in progress.
    Playing,
    /// Between games.
    Idle,
    /// Shutting down.
    Closing,
}

/// Why a proposal or active game ended without a normal result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalEndReason {
    /// Local validation rejected the proposal.
    ValidationRejected,
    /// Stopping was requested before agreement.
    StopRequested,
    /// The server reported rejection.
    Rejected,
    /// The connection ended.
    Disconnected,
    /// The session shut down.
    SessionShutdown,
}

/// An observed connection or login fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionEvent {
    /// The socket opened.
    Connected,
    /// Login succeeded.
    LoginAccepted,
    /// Login failed.
    LoginRejected,
    /// The connection ended.
    Disconnected,
}

/// One entry in the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordBody {
    /// A proposal or active game ended without a normal `game_end`.
    ProposalEnd {
        /// Actual `game_start` sequence, absent before local acceptance.
        game_start_seq: Option<u64>,
        /// Proposed identifier, including proposals that were rejected locally.
        proposal_game_id: Option<String>,
        /// Typed termination cause.
        reason: ProposalEndReason,
        /// Diagnostic context; wording is not stable.
        detail: Option<String>,
    },
    /// An engine spawn was requested, including replacement attempts.
    EngineGeneration {
        /// Per-run spawn attempt number, starting at one.
        generation: u64,
    },
    /// A session-level connection fact, independent of an active game.
    Connection {
        /// Connection or login event.
        event: ConnectionEvent,
    },
    /// The bridge started.
    BridgeStart {
        /// Bridge version.
        version: String,
        /// The engine command line, for reproducing the run.
        engine_cmd: String,
    },
    /// A game began, possibly with moves already played.
    GameStart {
        /// Player names.
        names: PlayerNames,
        /// Which side we play.
        my_color: Side,
        /// The position before any move.
        initial_sfen: String,
        /// Moves that arrived inside the proposal.
        moves_so_far: Vec<MoveRecord>,
        /// The time control we play under.
        time: TimeSummary,
        /// The entering-king rule we verify declarations against, spelled as the
        /// engine option value. Not an enum: the set comes from the proposal.
        entering_king_rule: String,
        /// `Max_Moves`, when the proposal stated one.
        max_moves: Option<u32>,
    },
    /// A move was played by either side.
    Move(MoveRecord),
    /// We asked the engine to search.
    Go {
        /// The move number the search answers.
        ply: u32,
        /// The times we told the engine it had.
        times: UsiGoTimes,
        /// The hard deadline, as monotonic milliseconds.
        deadline_at: Millis,
    },
    /// A search ran, or stopped running, on the opponent's clock.
    Ponder {
        /// What happened to it.
        outcome: PonderOutcome,
        /// The move number the search answers, counting the predicted move.
        ply: u32,
        /// The opponent move the search assumes, in USI spelling.
        predicted_usi: String,
        /// What the opponent actually played. Absent until they move.
        actual_usi: Option<String>,
        /// The hard deadline armed on a hit, as monotonic milliseconds.
        deadline_at: Option<Millis>,
    },
    /// The game ended.
    GameEnd {
        /// How it ended, from our side.
        result: GameResult,
        /// The terminal lines exactly as they arrived.
        terminal: Vec<String>,
        /// Black's remaining main time at the end.
        black_remaining_ms: u64,
        /// White's remaining main time at the end.
        white_remaining_ms: u64,
    },
    /// The session moved to a new phase.
    State {
        /// The phase now in force.
        phase: Phase,
    },
    /// Something went wrong.
    Alert {
        /// How loud.
        level: AlertLevel,
        /// What kind.
        code: AlertCode,
        /// Free text, not stable.
        detail: String,
    },
    /// The bridge was alive at this moment, and had nothing else to say.
    ///
    /// Written only to fill silence: if nothing has been logged for
    /// [`crate::config::HEARTBEAT_INTERVAL_MS`], one of these goes out. During a
    /// game the moves already prove liveness, so these appear mainly while
    /// waiting for a pairing or during a long search.
    ///
    /// The point is what a reader may conclude from their *absence*. A bridge
    /// killed mid-game leaves no terminal record and no alert — its log simply
    /// stops, indistinguishable from a game still being thought about. With this
    /// record the invariant becomes: a live bridge never leaves a gap longer than
    /// the interval, so a longer gap means it is gone.
    ///
    /// Carries no fields. The envelope's timestamp is the entire message, and
    /// the phase is already `State`'s to tell.
    Heartbeat,
    /// The bridge finished on purpose. Written last, after everything else.
    ///
    /// Distinct from `State { phase: Closing }`, which marks the *start* of
    /// shutting down: a bridge killed while closing leaves `Closing` as its final
    /// word. This record is what lets a reader say "this log is complete" rather
    /// than "this log stops here".
    BridgeStop,
    /// What the engine turned out to be, once it said so.
    ///
    /// Separate from `BridgeStart` on the merits, not for convenience.
    /// `BridgeStart` states how the bridge was *configured* at spawn — a command
    /// line and the bridge's own version. This states what the engine *is*, and
    /// it cannot be written at spawn because none of it is known until the USI
    /// handshake answers. Folding the two would also perpetuate the current
    /// confusion, where the only version in the log belongs to the bridge and is
    /// easily misread as the engine's.
    ///
    /// The options are recorded because a run is not reproducible without them:
    /// `Threads` and `USI_Hash` change what the engine plays, and until this
    /// record they existed only in the operator's shell history.
    EngineMeta {
        /// USI `id name`, absent if the engine never sent one.
        name: Option<String>,
        /// USI `id author`, absent if the engine never sent one.
        author: Option<String>,
        /// The **run-static** options applied at engine initialisation.
        ///
        /// Written once, between `usiok` and `isready`, and true for the whole
        /// run. Deliberately *not* "every `setoption` the bridge ever sends":
        /// per-game options are settled per game and belong to the game, not to
        /// the engine's identity.
        ///
        /// `EnteringKingRule` is the case that makes the distinction real. It is
        /// decided per game from the server's proposal, so the record that
        /// states it is [`RecordBody::GameStart::entering_king_rule`] — which is
        /// the single source of truth for what rule a given game was played
        /// under. Repeating a first-game value here would be a second, staler
        /// answer to a question `game_start` already answers correctly for every
        /// game.
        options: Vec<EngineOption>,
    },
}

/// One run-static `setoption` as applied, for reproducing a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineOption {
    /// Option name as sent.
    pub name: String,
    /// Value as sent.
    pub value: String,
}

/// A log entry with its envelope.
///
/// `at` is the monotonic value the shell supplied with the triggering event, so
/// records correlate with the wire log without the core reading a clock. The
/// shell adds the wall-clock timestamp and the schema version when it writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Monotonic increasing, gapless, per run.
    pub seq: u64,
    /// The game this belongs to, when one is in progress.
    pub game_id: Option<String>,
    /// The monotonic time of the event that produced this record.
    pub at: Millis,
    /// The record itself.
    #[serde(flatten)]
    pub body: RecordBody,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_serialize_with_a_type_tag() {
        let record = LogRecord {
            seq: 7,
            game_id: Some("game-1".to_owned()),
            at: 1_234,
            body: RecordBody::State { phase: Phase::Playing },
        };
        let json = serde_json::to_string(&record).expect("record should serialize");
        assert!(json.contains(r#""type":"state""#), "{json}");
        assert!(json.contains(r#""phase":"playing""#), "{json}");
        assert!(json.contains(r#""seq":7"#), "{json}");
    }

    #[test]
    fn alert_codes_are_snake_case_on_the_wire() {
        let json = serde_json::to_string(&AlertCode::SuspectResign).expect("code should serialize");
        assert_eq!(json, r#""suspect_resign""#);
        let json =
            serde_json::to_string(&AlertCode::MarginExhaustsBudget).expect("code should serialize");
        assert_eq!(json, r#""margin_exhausts_budget""#);
    }

    /// One line per record type, in the exact form the writer emits: the shell
    /// serializes to a `serde_json::Value`, adds `schema` and `ts`, and calls
    /// `to_string`, and a `Value` map is sorted, so the keys are alphabetical.
    ///
    /// These are hand-written rather than lifted from `logs/`, because a real
    /// event log carries player names and the engine's local path. The shapes
    /// match what floodgate produced on 2026-08-04.
    const WIRE_LINES: &[&str] = &[
        r#"{"at":0,"detail":null,"game_id":null,"game_start_seq":null,"proposal_game_id":"rejected-id","reason":"validation_rejected","schema":1,"seq":1,"ts":1785844602042,"type":"proposal_end"}"#,
        r#"{"at":0,"game_id":null,"generation":1,"schema":1,"seq":1,"ts":1785844602042,"type":"engine_generation"}"#,
        r#"{"at":0,"event":"connected","game_id":null,"schema":1,"seq":1,"ts":1785844602042,"type":"connection"}"#,
        r#"{"at":0,"engine_cmd":"/opt/engine/bin/engine","game_id":null,"schema":1,"seq":1,"ts":1785844602042,"type":"bridge_start","version":"0.1.0"}"#,
        r#"{"at":0,"game_id":null,"phase":"warming","schema":1,"seq":2,"ts":1785844602042,"type":"state"}"#,
        r#"{"at":199630,"entering_king_rule":"CSARule27","game_id":"server+testgame+alice+bob+20260804210004","initial_sfen":"lnsgkgsnl/1r5b1/ppppppppp/9/9/9/PPPPPPPPP/1B5R1/LNSGKGSNL b - 1","max_moves":512,"moves_so_far":[],"my_color":"black","names":{"black":"alice","white":"bob"},"schema":1,"seq":5,"time":{"byoyomi_ms":0,"inc_ms":10000,"least_ms":0,"roundup":false,"total_ms":300000,"unit_ms":1000},"ts":1785844801673,"type":"game_start"}"#,
        r#"{"at":199646,"deadline_at":507145,"game_id":"server+testgame+alice+bob+20260804210004","ply":0,"schema":1,"seq":8,"times":{"binc_ms":10000,"btime_ms":297500,"byoyomi_ms":null,"winc_ms":10000,"wtime_ms":300000},"ts":1785844801688,"type":"go"}"#,
        r#"{"at":214532,"black_remaining_ms":296000,"by":"engine","csa":"+2726FU","eval":{"cp":105,"depth":39,"mate":null,"nodes":14422438,"pv_usi":["2g2f","8c8d"]},"game_id":"server+testgame+alice+bob+20260804210004","ply":1,"schema":1,"seq":9,"side":"black","t_ms":14000,"ts":1785844816574,"type":"move","usi":"2g2f","white_remaining_ms":300000}"#,
        r#"{"actual_usi":null,"at":214532,"deadline_at":null,"game_id":"server+testgame+alice+bob+20260804210004","outcome":"started","ply":2,"predicted_usi":"8c8d","schema":1,"seq":10,"ts":1785844816574,"type":"ponder"}"#,
        // `"#` would end a single-hash raw string, and the terminal burst is full
        // of it. This is the same `#` hazard that kept the codec's fixture reader
        // off `assert_valid_transcript` (0002).
        r##"{"at":3919415,"black_remaining_ms":63000,"game_id":"server+testgame+alice+bob+20260804210004","result":"lose","schema":1,"seq":462,"terminal":["%TORYO","#RESIGN","#LOSE"],"ts":1785848521457,"type":"game_end","white_remaining_ms":184000}"##,
        r#"{"at":5150650,"code":"engine_dead","detail":"engine process ended","game_id":null,"level":"error","schema":1,"seq":466,"ts":1785849752692,"type":"alert"}"#,
        r#"{"at":5210650,"game_id":null,"schema":1,"seq":467,"ts":1785849812692,"type":"heartbeat"}"#,
        r#"{"at":5210651,"game_id":null,"schema":1,"seq":468,"ts":1785849812693,"type":"bridge_stop"}"#,
        r#"{"at":420,"author":"Motohiro Isozaki","game_id":null,"name":"YaneuraOu NNUE 8.30","options":[{"name":"Threads","value":"4"},{"name":"USI_Hash","value":"1024"}],"schema":1,"seq":2,"ts":1785844602420,"type":"engine_meta"}"#,
    ];

    /// The schema is a contract with an out-of-workspace reader, so what the
    /// session writes has to read back as the same record and re-serialize to
    /// the same bytes. Field renames and type changes both break here.
    ///
    /// Since `rsshogi-csa-watch` was retired (0011), no other crate in this
    /// workspace deserializes these records. This test is the whole enforcement.
    #[test]
    fn wire_lines_round_trip_byte_for_byte() {
        for line in WIRE_LINES {
            let record: LogRecord = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("should deserialize: {line}\n{err}"));

            let original: serde_json::Value =
                serde_json::from_str(line).expect("fixture should be valid json");
            let envelope = original.as_object().expect("fixture should be an object");

            let mut value = serde_json::to_value(&record).expect("record should serialize");
            let object = value.as_object_mut().expect("record should be an object");
            object.insert("schema".to_owned(), envelope["schema"].clone());
            object.insert("ts".to_owned(), envelope["ts"].clone());

            assert_eq!(&value.to_string(), line);
        }
    }

    /// Every variant has to appear above. A new record type that nobody pinned
    /// is how a reader starts disagreeing with the log.
    #[test]
    fn every_record_type_has_a_wire_line() {
        let mut seen: Vec<String> = WIRE_LINES
            .iter()
            .map(|line| {
                let value: serde_json::Value =
                    serde_json::from_str(line).expect("fixture should be valid json");
                value["type"].as_str().expect("fixture should have a type").to_owned()
            })
            .collect();
        seen.sort_unstable();

        assert_eq!(
            seen,
            [
                "alert",
                "bridge_start",
                "bridge_stop",
                "connection",
                "engine_generation",
                "engine_meta",
                "game_end",
                "game_start",
                "go",
                "heartbeat",
                "move",
                "ponder",
                "proposal_end",
                "state",
            ]
        );
    }
}
