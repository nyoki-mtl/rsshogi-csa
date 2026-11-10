//! The session core.
//!
//! One [`SessionCore::step`] consumes one [`Event`] and returns the [`Action`]s
//! the shell must perform. There is no I/O here, no clock reading, and no async
//! runtime type: the shell supplies monotonic milliseconds with every event.
//!
//! # Why one state machine
//!
//! The CSA session and the USI engine session are kept as plain state enums,
//! not as separate actors with their own loops. Every failure that loses a game
//! lives in the cross product of the two: the opponent's move arriving during a
//! search, a terminal burst arriving *instead of* our own move's echo, the
//! engine dying between `go` and `bestmove`. Split into two actors, that logic
//! lands in glue code that no transcript exercises.
//!
//! # Deadlines are pulled, not pushed
//!
//! [`SessionCore::next_deadline`] reports when the core next wants to be woken.
//! The shell recomputes it after every step and sleeps until then. There is no
//! timer identity and nothing to cancel, so a late wakeup is harmless by
//! construction. Deadlines are evaluated against the timestamp of *every* event,
//! not just [`Event::Tick`], so a deadline that passed while a server line was
//! in flight is still caught.

use rsshogi::types::EnteringKingRule;
use rsshogi_csa::{
    parse_line, Color, CsaCommand, CsaMove, EndMarker, EndMarkerKind, GameSummary,
    GameSummaryReader, MetaCommand, MoveLine, SpecialMove,
};
use rsshogi_usi::{
    BestMove, BestMoveKind, GoParams, InfoCommand, MateScore, ScoreValue, UsiCommand,
};

use crate::clock::MoveBudget;
use crate::config::{usi_entering_king_rule, BridgeConfig, PanicPolicy, HEARTBEAT_INTERVAL_MS};
use crate::game_state::{entering_king_rule_for, GameState};
use crate::probe::{info_body, ProbeRecord, SearchContext};
use crate::record::{
    AlertCode, AlertLevel, ConnectionEvent, EngineOption, EvalRecord, GameResult, LogRecord,
    Millis, MoveOrigin, MoveRecord, Phase, PlayerNames, PonderOutcome, ProposalEndReason,
    RecordBody, Side, TimeSummary,
};
use serde_json::json;

/// Something the outside world did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The bridge was asked to start.
    Started {
        /// Monotonic time.
        at: Millis,
    },
    /// The engine wrote a line.
    EngineLine {
        /// Monotonic time.
        at: Millis,
        /// The raw line, without its newline.
        line: String,
    },
    /// The engine process ended.
    EngineExited {
        /// Monotonic time.
        at: Millis,
    },
    /// The socket opened.
    Connected {
        /// Monotonic time.
        at: Millis,
    },
    /// The server wrote a line.
    ServerLine {
        /// Monotonic time.
        at: Millis,
        /// The raw line, without its newline.
        line: String,
    },
    /// The server closed the connection.
    ServerEof {
        /// Monotonic time.
        at: Millis,
    },
    /// The log writer committed a terminal record and its completion index.
    LogCommitted {
        /// Monotonic time.
        at: Millis,
        /// Source sequence of the committed `game_end` or `proposal_end`.
        seq: u64,
    },
    /// A scheduled wakeup, or any wakeup at all.
    Tick {
        /// Monotonic time.
        at: Millis,
    },
    /// The operator typed something.
    Operator {
        /// Monotonic time.
        at: Millis,
        /// What they asked for.
        command: OperatorCommand,
    },
}

impl Event {
    /// The monotonic timestamp the shell attached.
    #[must_use]
    pub const fn at(&self) -> Millis {
        match self {
            Self::Started { at }
            | Self::EngineLine { at, .. }
            | Self::EngineExited { at }
            | Self::Connected { at }
            | Self::ServerLine { at, .. }
            | Self::ServerEof { at }
            | Self::LogCommitted { at, .. }
            | Self::Tick { at }
            | Self::Operator { at, .. } => *at,
        }
    }
}

/// What the operator can ask for while a game is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorCommand {
    /// Resign the current game now.
    Resign,
    /// Finish this game, then shut down.
    StopAfterGame,
    /// Shut down immediately.
    Quit,
}

/// Something the shell must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start the engine process.
    SpawnEngine,
    /// Write a line to the engine.
    SendEngine(UsiCommand),
    /// Open the socket and log in.
    Connect,
    /// Write a line to the server.
    SendServer(CsaCommand),
    /// Append a record to the event log.
    Log(LogRecord),
    /// 観戦用の損失許容ログ。書込失敗を対局制御へ伝播させない。
    Probe(ProbeRecord),
    /// Stop everything.
    Shutdown {
        /// Why, for the operator.
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnginePhase {
    Cold,
    Handshaking,
    Warming,
    Ready,
    Busy,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerPhase {
    Idle,
    Connecting,
    LoggingIn,
    Waiting,
    ReadingSummary,
    AwaitingStart,
    Playing,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatorResignState {
    None,
    Pending,
    Sent,
}

/// Whose clock a search is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    /// Our own clock, for the position on the board. Its answer is played.
    Normal,
    /// The opponent's clock, for a position that assumes a move they have not
    /// played. Its answer is played only if that move turns out to be the one.
    Ponder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Search {
    kind: SearchKind,
    generation: u64,
    root_ply: u32,
    deadline_at: Option<Millis>,
    stop_sent_at: Option<Millis>,
    /// Set once the ladder has been walked for this search.
    ///
    /// The engine may still owe us a `bestmove`, so the search stays open, but
    /// its deadlines must stop firing: `at >= stop_sent_at + grace` remains true
    /// forever, and without this the hang would be reported on every event.
    abandoned: bool,
    /// 失効後も同じプロセスの返答を排出するまで次のgoを許可しない。
    drain_deadline: Option<Millis>,
}

/// The mate scale the engines report internally, and what these records use.
const MATE_SCORE: i32 = 32_000;

/// Maximum wait for terminal log completion before admitting another game.
const LOG_COMPLETION_TIMEOUT_MS: Millis = 5_000;

/// The opponent move a ponder search is betting on.
///
/// Both spellings are kept because they answer different questions: the CSA one
/// is compared against what the server sends, the USI one goes to the engine and
/// into the log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PredictedMove {
    csa: String,
    usi: String,
    /// The move number the search answers, counting the predicted move.
    ply: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SearchInfo {
    cp: Option<i32>,
    mate: Option<i32>,
    depth: Option<u32>,
    nodes: Option<u64>,
    pv: Vec<String>,
}

impl SearchInfo {
    fn absorb(&mut self, info: &InfoCommand) {
        if let Some(score) = &info.score {
            match &score.value {
                ScoreValue::Cp(cp) => {
                    self.cp = Some(*cp);
                    self.mate = None;
                }
                ScoreValue::Mate(mate) => {
                    self.mate = Some(match mate {
                        MateScore::Ply(ply) => *ply,
                        MateScore::UnknownWin => i32::MAX,
                        MateScore::UnknownLose => i32::MIN,
                    });
                    self.cp = None;
                }
            }
        }
        if info.depth.is_some() {
            self.depth = info.depth;
        }
        if info.nodes.is_some() {
            self.nodes = info.nodes;
        }
        if !info.pv.is_empty() {
            self.pv.clone_from(&info.pv);
        }
    }

    fn is_losing(&self, threshold_cp: i32) -> bool {
        if let Some(mate) = self.mate {
            return mate <= 0;
        }
        self.cp.is_some_and(|cp| cp <= threshold_cp)
    }

    /// The score a move comment carries, with mate folded onto the centipawn scale.
    ///
    /// Floodgate plots `,'* <score>` as the evaluation graph, and a mate score
    /// has no centipawn of its own. Without this the record loses its evaluation
    /// at exactly the moves worth looking at — the last few of every decisive
    /// game, measured on floodgate. `32000 - ply` is the scale the engines use
    /// internally, so a reader of these records already knows how to read it.
    fn comment_score(&self) -> Option<i32> {
        if let Some(cp) = self.cp {
            return Some(cp);
        }
        let mate = self.mate?;
        // `i32::MAX` and `i32::MIN` stand for a win or loss at an unstated
        // distance, which is as strong as the scale goes.
        let magnitude = if mate == i32::MAX || mate == i32::MIN {
            MATE_SCORE
        } else {
            MATE_SCORE.saturating_sub(mate.saturating_abs()).max(0)
        };
        Some(if mate > 0 { magnitude } else { -magnitude })
    }

    fn to_record(&self) -> EvalRecord {
        EvalRecord {
            cp: self.cp,
            mate: self.mate,
            depth: self.depth,
            nodes: self.nodes,
            pv_usi: self.pv.clone(),
        }
    }
}

/// The whole session, as one testable state machine.
///
/// The flags are independent facts about different subsystems — whether a search
/// is wanted, whether the socket is up, whether the operator asked to stop, and
/// whether we have shut down. Folding them into one enum would couple states that
/// genuinely vary on their own.
#[allow(clippy::struct_excessive_bools)]
pub struct SessionCore {
    config: BridgeConfig,
    engine_cmd: String,
    version: String,

    engine: EnginePhase,
    server: ServerPhase,

    reader: Option<GameSummaryReader>,
    summary_failure: Option<String>,
    summary: Option<GameSummary>,
    game: Option<GameState>,
    game_id: Option<String>,
    entering_king_rule: Option<EnteringKingRule>,
    engine_restarts: u32,
    engine_spawn_generation: u64,
    proposal_game_id: Option<String>,
    log_completion_barrier: bool,
    pending_log_seq: Option<u64>,
    pending_log_deadline: Option<Millis>,
    deferred_summary: Option<GameSummary>,

    /// `id name` / `id author`, collected between `usi` and `usiok`.
    engine_id_name: Option<String>,
    engine_id_author: Option<String>,
    /// Written once. A restarted engine re-announces itself, and repeating an
    /// unchanged identity would say something happened when nothing did.
    engine_meta_logged: bool,

    search: Option<Search>,
    probe_context: Option<SearchContext>,
    search_serial: u64,
    game_start_seq: u64,
    turn_started_at: Option<Millis>,
    turn_deadline: Option<Millis>,
    turn_fallback_deadline: Option<Millis>,
    engine_prepare_deadline: Option<Millis>,
    server_prepare_deadline: Option<Millis>,
    registration_pending: bool,
    proposal_pending: bool,
    local_reject_pending: bool,
    connect_attempts: u32,
    generation: u64,
    want_go: bool,
    info: SearchInfo,

    /// The `ponder` move from the `bestmove` we are currently sending.
    ///
    /// It cannot be used until the server echoes our move, because the ponder
    /// position is the board after both moves and the board only advances on
    /// the echo.
    ponder_hint: Option<String>,
    /// The opponent move the open ponder search assumes.
    predicted: Option<PredictedMove>,
    /// Set when a search was thrown away without waiting for its `bestmove`.
    ///
    /// The answer still arrives, and without this it looks like an anomaly
    /// rather than the tail of something we deliberately dropped.
    game_setup_pending: bool,

    pending_origin: Option<MoveOrigin>,
    pending_eval: Option<EvalRecord>,
    operator_resign: OperatorResignState,

    connected: bool,
    last_server_send_at: Millis,
    terminal: Vec<String>,
    degraded_streak: u32,
    games_played: u32,
    stop_after_game: bool,
    closed: bool,
    seq: u64,
    /// When the event log was last written to, for the heartbeat's silence rule.
    ///
    /// Every record moves this forward, so a busy game never pays for heartbeats
    /// it does not need — only real silence is filled.
    last_log_at: Millis,
}

impl SessionCore {
    /// A session that has not started yet.
    #[must_use]
    pub fn new(
        config: BridgeConfig,
        engine_cmd: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            config,
            engine_cmd: engine_cmd.into(),
            version: version.into(),
            engine: EnginePhase::Cold,
            server: ServerPhase::Idle,
            reader: None,
            summary_failure: None,
            summary: None,
            game: None,
            game_id: None,
            entering_king_rule: None,
            engine_restarts: 0,
            engine_spawn_generation: 0,
            proposal_game_id: None,
            log_completion_barrier: false,
            pending_log_seq: None,
            pending_log_deadline: None,
            deferred_summary: None,
            engine_id_name: None,
            engine_id_author: None,
            engine_meta_logged: false,
            search: None,
            probe_context: None,
            search_serial: 0,
            game_start_seq: 0,
            turn_started_at: None,
            turn_deadline: None,
            turn_fallback_deadline: None,
            engine_prepare_deadline: None,
            server_prepare_deadline: None,
            registration_pending: false,
            proposal_pending: false,
            local_reject_pending: false,
            connect_attempts: 0,
            generation: 0,
            want_go: false,
            info: SearchInfo::default(),
            ponder_hint: None,
            predicted: None,
            game_setup_pending: false,
            pending_origin: None,
            pending_eval: None,
            operator_resign: OperatorResignState::None,
            connected: false,
            last_server_send_at: 0,
            terminal: Vec::new(),
            degraded_streak: 0,
            games_played: 0,
            stop_after_game: false,
            closed: false,
            seq: 0,
            last_log_at: 0,
        }
    }

    /// Require terminal log completion before registering or accepting another game.
    ///
    /// The shell enables this before starting the session and supplies
    /// [`Event::LogCommitted`] after the terminal record and its index are flushed.
    /// Sans-I/O consumers retain the original admission behavior unless enabled.
    pub const fn enable_log_completion_barrier(&mut self) {
        self.log_completion_barrier = true;
    }

    /// When the core next wants to be woken, as monotonic milliseconds.
    ///
    /// The shell recomputes this after every [`SessionCore::step`]. A wakeup
    /// that arrives late, or one that arrives for a deadline that has since gone
    /// away, costs nothing: the core simply finds nothing to do.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Millis> {
        if self.closed {
            return None;
        }
        let mut best = merge_deadline(self.engine_prepare_deadline, self.server_prepare_deadline);
        best = merge_deadline(best, self.pending_log_deadline);
        if self.want_go {
            best = merge_deadline(best, self.turn_deadline);
        }
        if let Some(search) = self.search {
            best = merge_deadline(best, search.drain_deadline);
        }
        if let Some(search) = self.search.filter(|search| !search.abandoned) {
            let at = search.stop_sent_at.map_or(search.deadline_at, |stopped| {
                self.search_fallback_deadline(search, stopped)
            });
            best = merge_deadline(best, at);
        }
        if self.connected {
            let keepalive =
                self.last_server_send_at.saturating_add(self.config.effective_keepalive_ms());
            best = merge_deadline(best, Some(keepalive));
        }
        best
    }

    /// When a heartbeat would next be due, if nothing else is logged first.
    ///
    /// Deliberately not folded into [`Self::next_deadline`]. That reports the
    /// deadlines the *protocol* imposes — a move that must be sent, a keepalive
    /// the server expects — and the tests read it as exactly that. A heartbeat
    /// is a logging cadence, not a protocol obligation, so the shell keeps it as
    /// a separate floor on how long it may sleep.
    #[must_use]
    pub const fn next_heartbeat(&self) -> Option<Millis> {
        if self.closed {
            return None;
        }
        Some(self.last_log_at.saturating_add(HEARTBEAT_INTERVAL_MS))
    }

    /// Consumes one event and returns what the shell must do.
    pub fn step(&mut self, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        if self.closed {
            return out;
        }
        let at = event.at();
        // Late replies cannot extend preparation by advancing to the next phase.
        // An answer exactly at the boundary is accepted before checking timers.
        if self.engine_prepare_deadline.is_some_and(|end| at > end)
            || self.server_prepare_deadline.is_some_and(|end| at > end)
            || self.pending_log_deadline.is_some_and(|end| at > end)
        {
            let engine_event =
                matches!(event, Event::EngineLine { .. } | Event::EngineExited { .. });
            self.check_deadlines(at, &mut out);
            if self.closed || engine_event {
                return out;
            }
        }
        if self.search.is_some_and(|search| {
            search.kind == SearchKind::Normal
                && !search.abandoned
                && self.turn_fallback_deadline.is_some_and(|end| at > end)
        }) {
            self.fallback(at, &mut out);
        }
        self.dispatch(event, &mut out);
        if !self.closed {
            self.check_deadlines(at, &mut out);
        }
        out
    }

    fn dispatch(&mut self, event: Event, out: &mut Vec<Action>) {
        match event {
            Event::Started { at } => self.on_started(at, out),
            Event::EngineLine { at, line } => self.on_engine_line(at, &line, out),
            Event::EngineExited { at } => self.on_engine_exit(at, out),
            Event::Connected { at } => self.on_connected(at, out),
            Event::ServerLine { at, line } => self.on_server_line(at, &line, out),
            Event::ServerEof { at } => self.on_server_eof(at, out),
            Event::LogCommitted { at, seq } => self.on_log_committed(at, seq, out),
            Event::Tick { at } => self.on_tick(at, out),
            Event::Operator { at, command } => self.on_operator(at, command, out),
        }
    }

    fn on_log_committed(&mut self, at: Millis, seq: u64, out: &mut Vec<Action>) {
        if self.pending_log_seq != Some(seq) {
            return;
        }
        self.pending_log_seq = None;
        self.pending_log_deadline = None;
        if let Some(summary) = self.deferred_summary.take() {
            self.on_summary(at, summary, out);
        } else {
            self.maybe_register(at, out);
        }
    }

    /// Fill silence in the event log so a reader can tell us from a corpse.
    ///
    /// Killed mid-game, the bridge writes no terminal record and no alert; the
    /// log just stops, which reads exactly like a long think. Writing something
    /// every [`HEARTBEAT_INTERVAL_MS`] of quiet turns that ambiguity into an
    /// invariant a reader can use: a gap longer than the interval means the
    /// bridge is gone.
    ///
    /// Silence-filling rather than a fixed pulse, so a game in progress — where
    /// moves already prove liveness — costs nothing.
    fn on_tick(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.closed {
            return;
        }
        if at.saturating_sub(self.last_log_at) < HEARTBEAT_INTERVAL_MS {
            return;
        }
        self.log(at, RecordBody::Heartbeat, out);
    }

    // ---- startup -------------------------------------------------------

    fn on_started(&mut self, at: Millis, out: &mut Vec<Action>) {
        if let Err(reason) = self.config.validate() {
            self.shutdown(at, reason, out);
            return;
        }
        self.log(
            at,
            RecordBody::BridgeStart {
                version: self.version.clone(),
                engine_cmd: self.engine_cmd.clone(),
            },
            out,
        );
        self.log(at, RecordBody::State { phase: Phase::Warming }, out);
        self.spawn_engine(at, out);
        self.emit_probe(at, json!({"type":"engine","event":"starting"}), out);
        out.push(Action::SendEngine(UsiCommand::Usi));
        self.engine = EnginePhase::Handshaking;
        self.engine_prepare_deadline = Some(at.saturating_add(self.config.usi_timeout_ms));
    }

    /// Handles the engine going away, from either the reader task or a failed write.
    ///
    /// Both can report the same death, so this is idempotent: a second report
    /// while already dead would otherwise double-count the degraded streak and
    /// burn a restart.
    fn on_engine_exit(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.engine == EnginePhase::Dead {
            return;
        }
        self.engine = EnginePhase::Dead;
        self.engine_prepare_deadline = None;
        // Only a search on our own clock means we owe a move. Dying on the
        // opponent's clock owes nothing — whether idle or mid-ponder — and
        // walking the ladder there would resign a game we are still playing.
        let owed_a_move = self.search.is_some_and(|search| {
            search.kind == SearchKind::Normal && search.generation == self.generation
        });
        self.ponder_hint = None;
        self.predicted = None;
        self.alert(at, AlertLevel::Error, AlertCode::EngineDead, "engine process ended", out);
        self.emit_probe(at, json!({"type":"engine","event":"exited"}), out);
        if owed_a_move {
            self.fallback(at, out);
        }
        self.search = None;
        self.probe_search(at, "ended", "finished", Some("engine_exit"), out);
        self.probe_context = None;
        self.info = SearchInfo::default();
        if self.game.is_none() {
            // Nothing is at stake between games, so there is no reason to give
            // up the session over it. Shutting down here means never playing.
            self.restart_engine(at, out);
            if self.engine != EnginePhase::Handshaking {
                self.shutdown(at, "engine process ended and cannot be restarted", out);
            }
            return;
        }
        self.restart_engine(at, out);
    }

    /// Brings a replacement engine up in the middle of a game.
    ///
    /// `usinewgame` is deliberately not sent: the game is already in progress
    /// and every `go` restates the position anyway. The entering-king rule is
    /// re-sent because it came from the proposal, not from the static options.
    fn restart_engine(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.engine_restarts >= self.config.max_engine_restarts {
            return;
        }
        self.engine_restarts = self.engine_restarts.saturating_add(1);
        let detail = format!(
            "restarting the engine ({} of {})",
            self.engine_restarts, self.config.max_engine_restarts
        );
        self.alert(at, AlertLevel::Warn, AlertCode::EngineRestarted, &detail, out);
        self.spawn_engine(at, out);
        self.emit_probe(at, json!({"type":"engine","event":"starting"}), out);
        out.push(Action::SendEngine(UsiCommand::Usi));
        self.engine = EnginePhase::Handshaking;
        self.engine_prepare_deadline = Some(at.saturating_add(self.config.usi_timeout_ms));
    }

    // ---- engine --------------------------------------------------------

    fn on_engine_line(&mut self, at: Millis, line: &str, out: &mut Vec<Action>) {
        let Ok(command) = parse_usi(line) else {
            self.alert(at, AlertLevel::Warn, AlertCode::UnreadableEngineLine, line, out);
            return;
        };
        match command {
            UsiCommand::Id { key, value } => match key.as_str() {
                "name" => self.engine_id_name = Some(value),
                "author" => self.engine_id_author = Some(value),
                _ => {}
            },
            UsiCommand::UsiOk => self.on_usiok(at, out),
            UsiCommand::ReadyOk => self.on_readyok(at, out),
            UsiCommand::Info(info) => {
                self.emit_probe(at, info_body(&info), out);
                if self
                    .search
                    .is_some_and(|search| search.generation == self.generation && !search.abandoned)
                {
                    self.info.absorb(&info);
                }
            }
            UsiCommand::BestMove(best) => self.on_bestmove(at, best, out),
            _ => {}
        }
    }

    fn on_usiok(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.engine != EnginePhase::Handshaking {
            return;
        }
        let mut applied: Vec<EngineOption> = Vec::new();
        for (name, value) in &self.config.engine_options {
            out.push(Action::SendEngine(UsiCommand::SetOption {
                name: name.clone(),
                value: Some(value.clone()),
            }));
            applied.push(EngineOption { name: name.clone(), value: value.clone() });
        }
        if let Some(rule) = self.entering_king_rule {
            out.push(Action::SendEngine(UsiCommand::SetOption {
                name: "EnteringKingRule".to_owned(),
                value: Some(usi_entering_king_rule(rule).to_owned()),
            }));
            // Sent, but deliberately not recorded here. The rule is decided per
            // game from the server's proposal, so `game_start.entering_king_rule`
            // is its single source of truth — and it is right for every game,
            // where a copy taken at initialisation would be right only for the
            // first. `engine_meta.options` is the run-static set.
        }
        // Written here, after `usiok`, because that is when the identity is
        // complete and the run-static option list is settled — and recorded as
        // sent rather than as configured, so the log states what the engine was
        // actually told.
        if !self.engine_meta_logged {
            self.engine_meta_logged = true;
            let body = RecordBody::EngineMeta {
                name: self.engine_id_name.clone(),
                author: self.engine_id_author.clone(),
                options: applied,
            };
            self.log(at, body, out);
        }
        out.push(Action::SendEngine(UsiCommand::IsReady));
        self.engine = EnginePhase::Warming;
        self.engine_prepare_deadline = Some(at.saturating_add(self.config.ready_timeout_ms));
    }

    /// Warm-up finishes before `LOGIN`, never after.
    ///
    /// A GPU engine builds its backend inside `isready`, and the server gives a
    /// proposal only 120 seconds to be agreed. Connecting first would risk
    /// spending that window on warm-up.
    fn on_readyok(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.engine != EnginePhase::Warming {
            return;
        }
        self.engine = EnginePhase::Ready;
        self.engine_prepare_deadline = None;
        self.emit_probe(at, json!({"type":"engine","event":"ready"}), out);
        self.prepare_game(out);
        if self.connected || self.server != ServerPhase::Idle {
            self.maybe_go(at, out);
            return;
        }
        self.log(at, RecordBody::State { phase: Phase::Connecting }, out);
        self.server = ServerPhase::Connecting;
        self.connect_attempts = self.connect_attempts.saturating_add(1);
        self.server_prepare_deadline = Some(at.saturating_add(30_000));
        out.push(Action::Connect);
    }

    fn on_bestmove(&mut self, at: Millis, best: BestMove, out: &mut Vec<Action>) {
        let oversized = matches!(&best.bestmove, BestMoveKind::Move(usi) if usi.len() > 64)
            || best.ponder.as_ref().is_some_and(|usi| usi.len() > 64);
        if oversized {
            out.push(Action::Probe(ProbeRecord {
                at,
                context: None,
                body: json!({"type":"unattributed","reason":"oversized_bestmove","truncated_fields":["bestmove"]}),
            }));
        } else {
            let result = match &best.bestmove {
                BestMoveKind::Move(usi) => json!({"kind":"move","usi":usi}),
                BestMoveKind::Resign => json!({"kind":"resign"}),
                BestMoveKind::Win => json!({"kind":"win"}),
            };
            let kind = if self.probe_context.is_some() { "bestmove" } else { "unattributed" };
            self.emit_probe(at, json!({"type":kind,"reason":if kind == "unattributed" { Some("no_search") } else { None },"bestmove":result,"ponder":best.ponder}), out);
        }
        self.probe_search(at, "ended", "finished", Some("bestmove"), out);
        self.probe_context = None;
        let Some(search) = self.search.take() else {
            self.alert(at, AlertLevel::Warn, AlertCode::StaleBestmove, "no search was open", out);
            return;
        };
        self.engine = EnginePhase::Ready;

        if search.generation != self.generation {
            // A ponder whose prediction failed is expected to answer, and its
            // answer being useless is the normal course of things.
            if search.kind != SearchKind::Ponder && search.drain_deadline.is_none() {
                self.alert(
                    at,
                    AlertLevel::Warn,
                    AlertCode::StaleBestmove,
                    "answer to an abandoned search",
                    out,
                );
            }
            self.info = SearchInfo::default();
            self.prepare_game(out);
            self.maybe_go(at, out);
            return;
        }

        if search.kind == SearchKind::Ponder {
            // The prediction never resolved — the game ended, or the session was
            // torn down, while the search was still on the opponent's clock.
            // `ponderhit` is the only thing that turns a ponder into an answer we
            // are allowed to play, and it did not happen.
            self.predicted = None;
            self.maybe_go(at, out);
            return;
        }

        if search.stop_sent_at.is_some() {
            self.alert(at, AlertLevel::Warn, AlertCode::EngineSlow, "answered inside grace", out);
        }

        let Some(game) = &self.game else {
            self.alert(at, AlertLevel::Warn, AlertCode::StaleBestmove, "no game is open", out);
            return;
        };

        match best.bestmove {
            BestMoveKind::Move(usi) => match game.csa_for_usi(&usi) {
                Ok(csa) => {
                    self.degraded_streak = 0;
                    // Held until the server echoes our move; only then does the
                    // board reach the position this predicts from.
                    self.ponder_hint.clone_from(&best.ponder);
                    self.send_our_move(at, &csa, MoveOrigin::Engine, out);
                }
                Err(err) => {
                    self.alert(
                        at,
                        AlertLevel::Error,
                        AlertCode::IllegalBestmove,
                        &err.to_string(),
                        out,
                    );
                    self.search = Some(search);
                    self.fallback(at, out);
                    self.search = None;
                }
            },
            BestMoveKind::Win => {
                // Checked, but not overruled. Our checker is the same `rsshogi`
                // code the engine is built on, so a disagreement is as likely to
                // be ours; and the ladder below here would resign a game the
                // engine believes it has already won, which is worse than any
                // mistaken declaration. A declaration the summary rule does not
                // support is worth shouting about, not worth vetoing.
                if !game.can_declare_win() {
                    self.alert(
                        at,
                        AlertLevel::Error,
                        AlertCode::UnverifiedDeclaration,
                        "declaration does not pass our own check of the summary rule",
                        out,
                    );
                }
                self.degraded_streak = 0;
                self.send_special(at, SpecialMove::Kachi, out);
            }
            BestMoveKind::Resign => {
                // When to give up is the engine's decision, and it is passed on
                // whatever the search said. A resign the score does not explain
                // is worth finding in the log afterwards; substituting our own
                // move for it would only be a guess dressed as a rescue.
                if !self.info.is_losing(self.config.resign_threshold_cp) && game.has_legal_moves() {
                    self.alert(
                        at,
                        AlertLevel::Error,
                        AlertCode::SuspectResign,
                        "resign from a position the engine did not report as lost",
                        out,
                    );
                }
                self.send_special(at, SpecialMove::Toryo, out);
            }
        }
    }

    // ---- server --------------------------------------------------------

    fn on_connected(&mut self, at: Millis, out: &mut Vec<Action>) {
        self.connection(at, ConnectionEvent::Connected, out);
        self.connected = true;
        self.last_server_send_at = at;
        self.server = ServerPhase::LoggingIn;
        self.server_prepare_deadline = Some(at.saturating_add(self.config.login_timeout_ms));
        self.send_server(
            at,
            CsaCommand::Login {
                name: self.config.login_name.clone(),
                password: self.config.password.clone(),
                x1: self.config.x1,
            },
            out,
        );
    }

    fn on_server_line(&mut self, at: Millis, line: &str, out: &mut Vec<Action>) {
        if self.server == ServerPhase::ReadingSummary {
            self.feed_summary(at, line, out);
            return;
        }
        let command = match parse_line(line) {
            Ok(command) => command,
            Err(err) => {
                self.alert(
                    at,
                    AlertLevel::Warn,
                    AlertCode::UnreadableServerLine,
                    &err.to_string(),
                    out,
                );
                return;
            }
        };
        self.on_server_command(at, command, out);
    }

    fn on_server_command(&mut self, at: Millis, command: CsaCommand, out: &mut Vec<Action>) {
        match command {
            CsaCommand::LoginOk { .. } => {
                self.connection(at, ConnectionEvent::LoginAccepted, out);
                self.connect_attempts = 0;
                self.server_prepare_deadline = None;
                self.server = ServerPhase::Waiting;
                self.log(at, RecordBody::State { phase: Phase::WaitingPairing }, out);
                self.registration_pending = true;
                self.maybe_register(at, out);
            }
            CsaCommand::LoginIncorrect => {
                self.connection(at, ConnectionEvent::LoginRejected, out);
                self.alert(at, AlertLevel::Error, AlertCode::LoginFailed, "login refused", out);
                self.shutdown(at, "login refused", out);
            }
            CsaCommand::BeginGameSummary => {
                self.registration_pending = false;
                self.proposal_pending = true;
                self.proposal_game_id = None;
                self.server = ServerPhase::ReadingSummary;
                self.reader = Some(GameSummaryReader::new());
                self.summary_failure = None;
                self.feed_summary(at, "BEGIN Game_Summary", out);
            }
            CsaCommand::RejectNotice { by, .. } => {
                if !self.proposal_pending && !self.local_reject_pending {
                    return;
                }
                self.end_proposal(
                    at,
                    ProposalEndReason::Rejected,
                    Some(format!("proposal rejected by {by}")),
                    out,
                );
                self.alert(
                    at,
                    AlertLevel::Warn,
                    AlertCode::UnusableProposal,
                    &format!("proposal rejected by {by}"),
                    out,
                );
                self.reset_game(at, out);
                self.server = ServerPhase::Waiting;
                self.registration_pending = true;
                if !self.should_continue() {
                    self.shutdown(at, "proposal rejected and no further game requested", out);
                }
            }
            CsaCommand::Start { game_id } => self.on_start(at, game_id, out),
            CsaCommand::Move(line) => self.on_server_move(at, &line, out),
            CsaCommand::Special { kind, .. } if self.server == ServerPhase::Playing => {
                self.terminal.push(format!("%{}", kind.as_token()));
            }
            CsaCommand::End(marker) => self.on_end_marker(at, &marker, out),
            CsaCommand::LogoutCompleted => self.shutdown(at, "logout completed", out),
            _ => {}
        }
    }

    fn feed_summary(&mut self, at: Millis, line: &str, out: &mut Vec<Action>) {
        if let Some(game_id) = line.strip_prefix("Game_ID:") {
            self.proposal_game_id = Some(game_id.to_owned());
        }
        if self.summary_failure.is_some() {
            if line.trim() == "END Game_Summary" {
                if let Some(detail) = self.summary_failure.take() {
                    self.reject_proposal(
                        at,
                        None,
                        &detail,
                        ProposalEndReason::ValidationRejected,
                        out,
                    );
                }
            }
            return;
        }

        let Some(reader) = &mut self.reader else {
            return;
        };
        let done = match reader.feed(line) {
            Ok(done) => done,
            Err(err) => {
                let detail = err.to_string();
                self.alert(at, AlertLevel::Warn, AlertCode::UnreadableServerLine, &detail, out);
                self.reader = None;
                if line.trim() == "END Game_Summary" {
                    self.reject_proposal(
                        at,
                        None,
                        &detail,
                        ProposalEndReason::ValidationRejected,
                        out,
                    );
                } else {
                    self.summary_failure = Some(detail);
                }
                return;
            }
        };
        let Some(summary) = done else {
            return;
        };
        self.reader = None;
        self.on_summary(at, summary, out);
    }

    /// Turns down a proposal we cannot play, and goes back to waiting.
    ///
    /// Declining costs one game. Agreeing to a game we have misread costs that
    /// game too, by illegal move, after wasting the clock.
    fn reject_proposal(
        &mut self,
        at: Millis,
        game_id: Option<String>,
        detail: &str,
        reason: ProposalEndReason,
        out: &mut Vec<Action>,
    ) {
        if game_id.is_some() {
            self.proposal_game_id.clone_from(&game_id);
        }
        self.end_proposal(at, reason, Some(detail.to_owned()), out);
        self.alert(at, AlertLevel::Error, AlertCode::UnusableProposal, detail, out);
        self.send_server(at, CsaCommand::Reject { game_id }, out);
        self.reset_game(at, out);
        // The local termination is already logged, but the server still owes
        // REJECT before explicit x1 registration is safe again.
        self.local_reject_pending = true;
        self.server = ServerPhase::Waiting;
        if !self.should_continue() {
            self.shutdown(at, "proposal unusable and no further game requested", out);
        }
    }

    fn on_summary(&mut self, at: Millis, summary: GameSummary, out: &mut Vec<Action>) {
        self.proposal_game_id.clone_from(&summary.game_id);
        if self.stop_after_game {
            self.reject_proposal(
                at,
                summary.game_id.clone(),
                "stopped before agreeing",
                ProposalEndReason::StopRequested,
                out,
            );
            return;
        }
        if self.pending_log_seq.is_some() {
            self.deferred_summary = Some(summary);
            return;
        }
        let rule =
            self.config.entering_king_rule.unwrap_or_else(|| entering_king_rule_for(&summary));
        let (game, replayed_records) = match game_from_proposal(&summary, rule) {
            Ok(prepared) => prepared,
            Err(detail) => {
                self.reject_proposal(
                    at,
                    summary.game_id.clone(),
                    &detail,
                    ProposalEndReason::ValidationRejected,
                    out,
                );
                return;
            }
        };

        self.game_id.clone_from(&summary.game_id);
        self.entering_king_rule = Some(rule);
        match game.ledger().budget_ms(game.our_color()) {
            Some(budget_ms) if budget_ms <= self.config.margin_ms => {
                let detail = format!(
                    "--margin-ms {} consumes our {budget_ms} ms move budget; search deadline will be immediate",
                    self.config.margin_ms
                );
                self.alert(at, AlertLevel::Warn, AlertCode::MarginExhaustsBudget, &detail, out);
            }
            Some(_) | None => {}
        }
        self.game_setup_pending = true;

        self.log(
            at,
            RecordBody::GameStart {
                names: PlayerNames {
                    black: summary.name_black.clone().unwrap_or_default(),
                    white: summary.name_white.clone().unwrap_or_default(),
                },
                my_color: record_side(game.our_color()),
                initial_sfen: game.initial_sfen().to_owned(),
                moves_so_far: replayed_records,
                time: time_summary(&game),
                entering_king_rule: usi_entering_king_rule(rule).to_owned(),
                max_moves: summary.max_moves,
            },
            out,
        );
        self.game_start_seq = self.seq;

        self.game = Some(game);
        self.prepare_game(out);
        self.summary = Some(summary);
        self.terminal.clear();
        self.degraded_streak = 0;
        self.server = ServerPhase::AwaitingStart;
        self.log(at, RecordBody::State { phase: Phase::Agreed }, out);

        let game_id = self.game_id.clone();
        self.send_server(at, CsaCommand::Agree { game_id }, out);
    }

    fn on_start(&mut self, at: Millis, game_id: String, out: &mut Vec<Action>) {
        self.proposal_pending = false;
        self.game_id = Some(game_id);
        self.server = ServerPhase::Playing;
        self.log(at, RecordBody::State { phase: Phase::Playing }, out);
        if self.game.as_ref().is_some_and(GameState::is_our_turn) {
            self.begin_turn(at);
            self.want_go = true;
            self.maybe_go(at, out);
        }
    }

    fn on_server_move(&mut self, at: Millis, line: &MoveLine, out: &mut Vec<Action>) {
        if self.server != ServerPhase::Playing {
            return;
        }
        let charged = line.elapsed.unwrap_or(0);
        // Judged before the board moves, because the prediction is a move in the
        // position as it stands right now.
        let ponder_hit = self.ponder_verdict(line.csa.as_str());
        let Some(game) = &mut self.game else {
            return;
        };
        let played = match game.apply_csa_move(line.csa.as_str(), charged) {
            Ok(played) => played,
            Err(err) => {
                self.alert(
                    at,
                    AlertLevel::Error,
                    AlertCode::UnexpectedServerMove,
                    &err.to_string(),
                    out,
                );
                return;
            }
        };

        let ours = played.color == game.our_color();
        if ours {
            self.turn_started_at = None;
            self.turn_deadline = None;
        } else {
            self.begin_turn(at);
        }
        let origin = if ours {
            self.pending_origin.take().unwrap_or(MoveOrigin::Engine)
        } else {
            MoveOrigin::Opponent
        };
        let eval = if ours { self.pending_eval.take() } else { None };
        self.log(at, RecordBody::Move(move_record(&played, origin, eval)), out);

        if self.operator_resign == OperatorResignState::Pending {
            self.maybe_send_operator_resign(at, out);
            return;
        }

        match ponder_hit {
            // The search already running is the search for this position, and
            // it now runs on our clock instead of theirs.
            Some(true) => {
                self.on_ponder_hit(at, out);
                return;
            }
            Some(false) => self.abandon_ponder(at, &played.usi, out),
            None => {}
        }

        if self.game.as_ref().is_some_and(GameState::is_our_turn) {
            self.want_go = true;
            // A ponder we just abandoned is still open, so this does nothing
            // until its `bestmove` arrives and reopens the way.
            self.maybe_go(at, out);
        } else if ours {
            self.maybe_ponder(at, out);
        }
    }

    fn on_end_marker(&mut self, at: Millis, marker: &EndMarker, out: &mut Vec<Action>) {
        if self.server != ServerPhase::Playing && self.server != ServerPhase::AwaitingStart {
            return;
        }
        self.terminal.push(format!("#{}", marker.as_token()));
        let final_marker =
            matches!(marker.kind(), EndMarkerKind::Outcome) || matches!(marker, EndMarker::Chudan);
        if final_marker {
            self.finish_game(at, marker, out);
        }
    }

    fn on_server_eof(&mut self, at: Millis, out: &mut Vec<Action>) {
        self.connection(at, ConnectionEvent::Disconnected, out);
        self.end_proposal(at, ProposalEndReason::Disconnected, None, out);
        let had_proposal = self.game.is_some()
            || self.reader.is_some()
            || self.summary_failure.is_some()
            || self.deferred_summary.is_some()
            || self.local_reject_pending;
        self.connected = false;
        self.server_prepare_deadline = None;
        self.registration_pending = false;
        self.proposal_pending = false;
        if self.server == ServerPhase::Playing {
            // Dropping the socket mid-game is a loss on every CSA server, so this
            // is never a retry, only a report.
            self.alert(
                at,
                AlertLevel::Error,
                AlertCode::ConnectionLost,
                "connection closed during a game",
                out,
            );
            self.stop_search(at, out);
            self.reset_game(at, out);
            self.shutdown(at, "connection closed during a game", out);
            return;
        }
        if had_proposal {
            self.reset_game(at, out);
        }
        if self.should_continue() {
            if self.connect_attempts >= 3 {
                self.shutdown(at, "connection attempts exhausted", out);
                return;
            }
            self.connect_attempts = self.connect_attempts.saturating_add(1);
            self.server = ServerPhase::Connecting;
            self.server_prepare_deadline = Some(at.saturating_add(30_000));
            self.log(at, RecordBody::State { phase: Phase::Connecting }, out);
            out.push(Action::Connect);
        } else {
            self.shutdown(at, "no more games requested", out);
        }
    }

    fn on_operator(&mut self, at: Millis, command: OperatorCommand, out: &mut Vec<Action>) {
        match command {
            OperatorCommand::Resign => {
                if self.server == ServerPhase::Playing
                    && self.operator_resign == OperatorResignState::None
                {
                    self.operator_resign = OperatorResignState::Pending;
                    self.maybe_send_operator_resign(at, out);
                }
            }
            OperatorCommand::StopAfterGame => {
                self.stop_after_game = true;
                if let Some(summary) = self.deferred_summary.take() {
                    self.on_summary(at, summary, out);
                } else if self.game.is_none() && self.server != ServerPhase::ReadingSummary {
                    self.shutdown(at, "no game remains to finish", out);
                }
            }
            OperatorCommand::Quit => {
                self.stop_search(at, out);
                self.shutdown(at, "operator requested shutdown", out);
            }
        }
    }

    // ---- searching -----------------------------------------------------

    fn emit_probe(&self, at: Millis, body: serde_json::Value, out: &mut Vec<Action>) {
        out.push(Action::Probe(ProbeRecord { at, context: self.probe_context.clone(), body }));
    }

    fn probe_search(
        &mut self,
        at: Millis,
        event: &str,
        state: &str,
        reason: Option<&str>,
        out: &mut Vec<Action>,
    ) {
        if let Some(context) = &mut self.probe_context {
            context.state = state.into();
            self.emit_probe(at, json!({"type":"search","event":event,"reason":reason}), out);
        }
    }

    fn begin_probe(&mut self, at: Millis, predicted: Option<&str>, out: &mut Vec<Action>) {
        self.search_serial = self.search_serial.saturating_add(1);
        self.probe_context = None;
        let Some(game) = &self.game else { return };
        let mut root = game.position().clone();
        if let Some(usi) = predicted {
            let Some(mv) = rsshogi::board::move_from_usi(&root, usi) else { return };
            if !root.is_legal_move32(mv) {
                return;
            }
            root.apply_move32(mv);
        }
        let Some(game_id) = self.game_id.as_ref().filter(|id| id.len() <= 4096) else { return };
        self.probe_context = Some(SearchContext {
            game_start_seq: self.game_start_seq.to_string(),
            game_id: game_id.clone(),
            search_id: self.search_serial.to_string(),
            root_sfen: root.to_sfen(None),
            root_side: match root.turn() {
                rsshogi::types::Color::Black => Side::Black,
                rsshogi::types::Color::White => Side::White,
            },
            base_ply: game.ply(),
            predicted_usi: predicted.map(str::to_owned),
            origin: if predicted.is_some() { "ponder" } else { "normal" }.into(),
            state: if predicted.is_some() { "pondering" } else { "thinking" }.into(),
            ponder_outcome: predicted.map(|_| "pending".into()),
        });
        self.emit_probe(at, json!({"type":"search","event":"started","reason":null}), out);
    }

    fn begin_turn(&mut self, at: Millis) {
        self.turn_started_at = Some(at);
        self.turn_deadline = self.move_deadline(at);
        self.turn_fallback_deadline = self.game.as_ref().and_then(|game| {
            game.ledger().budget_ms(game.our_color()).map(|budget| {
                let reserve = self.config.margin_ms.saturating_sub(self.config.stop_grace_ms);
                // If even the margin consumes the whole budget, do not wait.
                let usable = if budget <= self.config.margin_ms { 0 } else { budget - reserve };
                at.saturating_add(usable)
            })
        });
    }

    fn search_fallback_deadline(&self, search: Search, stopped: Millis) -> Option<Millis> {
        let grace = Some(stopped.saturating_add(self.config.stop_grace_ms));
        if search.kind == SearchKind::Normal {
            merge_deadline(grace, self.turn_fallback_deadline)
        } else {
            grace
        }
    }

    /// When the engine must have answered for the position on the board.
    ///
    /// `None` means the time control does not bound it. The margin is taken off
    /// here rather than trusted to the engine, because the engine only sees the
    /// numbers we told it and the server's flooring is not one of them.
    fn move_deadline(&self, at: Millis) -> Option<Millis> {
        let at = self.turn_started_at.unwrap_or(at);
        let game = self.game.as_ref()?;
        match game.ledger().budget(game.our_color()) {
            MoveBudget::Unlimited => None,
            MoveBudget::Expired => Some(at),
            MoveBudget::Units(_) => game
                .ledger()
                .budget_ms(game.our_color())
                .map(|budget| at.saturating_add(budget.saturating_sub(self.config.margin_ms))),
        }
    }

    fn maybe_go(&mut self, at: Millis, out: &mut Vec<Action>) {
        if !self.want_go
            || self.server != ServerPhase::Playing
            || self.search.is_some()
            || self.operator_resign != OperatorResignState::None
        {
            return;
        }
        // A fresh search of our own supersedes anything the last `bestmove`
        // offered to ponder on.
        self.ponder_hint = None;
        if self.engine == EnginePhase::Dead {
            // Sitting here burns the whole clock and loses anyway, minutes
            // later, while looking like a hang to whoever is watching.
            self.want_go = false;
            self.alert(
                at,
                AlertLevel::Error,
                AlertCode::EngineDead,
                "no engine is available to answer this move",
                out,
            );
            self.send_special(at, SpecialMove::Toryo, out);
            return;
        }
        if self.engine != EnginePhase::Ready {
            // Warming up after a restart. The move is still owed, and
            // `on_readyok` is what finally issues it.
            return;
        }
        if self.turn_deadline.is_some_and(|end| at >= end) {
            self.fallback(at, out);
            return;
        }
        self.prepare_game(out);
        let Some(game) = &self.game else {
            return;
        };
        if !game.is_our_turn() {
            self.want_go = false;
            return;
        }

        self.want_go = false;
        self.generation = self.generation.saturating_add(1);
        self.info = SearchInfo::default();

        let elapsed = self.turn_started_at.map_or(0, |started| at.saturating_sub(started));
        let times = game.usi_times(self.config.margin_ms.saturating_add(elapsed));
        let root_ply = game.ply();
        let deadline_at = self.move_deadline(at);
        self.begin_probe(at, None, out);
        let game = self.game.as_ref().expect("probe does not change the game");

        if let Ok(position) = parse_line_usi(&game.usi_position_command()) {
            out.push(Action::SendEngine(position));
        }
        out.push(Action::SendEngine(UsiCommand::Go(GoParams {
            btime: Some(times.btime_ms),
            wtime: Some(times.wtime_ms),
            byoyomi: times.byoyomi_ms,
            binc: times.binc_ms,
            winc: times.winc_ms,
            ..GoParams::default()
        })));

        self.engine = EnginePhase::Busy;
        self.search = Some(Search {
            kind: SearchKind::Normal,
            generation: self.generation,
            root_ply,
            deadline_at,
            stop_sent_at: None,
            abandoned: false,
            drain_deadline: None,
        });
        self.log(
            at,
            RecordBody::Go { ply: root_ply, times, deadline_at: deadline_at.unwrap_or(0) },
            out,
        );
    }

    // ---- pondering -----------------------------------------------------

    /// Starts a search on the opponent's clock, if there is anything to search.
    ///
    /// Called once our own move has been echoed, because the board only advances
    /// on the echo and the ponder position is the board after both moves. The
    /// echo costs a few tens of milliseconds on floodgate, which is a fair price
    /// for pondering a position we know we are actually in.
    ///
    /// There is no separate switch for this: `USI_Ponder` is the only one, read
    /// back out of the engine options. See [`BridgeConfig::ponder_enabled`] for
    /// why the bridge enforces it instead of leaving it to the engine.
    fn maybe_ponder(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.operator_resign != OperatorResignState::None {
            return;
        }
        let Some(usi) = self.ponder_hint.take() else {
            return;
        };
        if !self.config.ponder_enabled()
            || self.server != ServerPhase::Playing
            || self.search.is_some()
            || self.engine != EnginePhase::Ready
        {
            return;
        }
        let Some(game) = &self.game else {
            return;
        };
        if game.is_our_turn() {
            return;
        }

        // The same gate our own moves pass, applied to the opponent's move this
        // time. A prediction the board rejects would put the engine on a
        // position that does not exist.
        let csa = match game.csa_for_usi(&usi) {
            Ok(csa) => csa,
            Err(err) => {
                self.alert(at, AlertLevel::Warn, AlertCode::UnusablePonder, &err.to_string(), out);
                return;
            }
        };

        let position = game.usi_position_command_with(&usi);
        let times = game.usi_times(self.config.margin_ms);
        let ply = game.ply().saturating_add(1);
        self.begin_probe(at, Some(&usi), out);

        self.generation = self.generation.saturating_add(1);
        self.info = SearchInfo::default();

        if let Ok(command) = parse_line_usi(&position) {
            out.push(Action::SendEngine(command));
        }
        out.push(Action::SendEngine(UsiCommand::Go(GoParams {
            ponder: true,
            btime: Some(times.btime_ms),
            wtime: Some(times.wtime_ms),
            byoyomi: times.byoyomi_ms,
            binc: times.binc_ms,
            winc: times.winc_ms,
            ..GoParams::default()
        })));

        self.engine = EnginePhase::Busy;
        self.search = Some(Search {
            kind: SearchKind::Ponder,
            generation: self.generation,
            root_ply: ply,
            // Nothing is owed while the opponent thinks, so nothing is due.
            deadline_at: None,
            stop_sent_at: None,
            abandoned: false,
            drain_deadline: None,
        });
        self.predicted = Some(PredictedMove { csa, usi: usi.clone(), ply });
        self.log(
            at,
            RecordBody::Ponder {
                outcome: PonderOutcome::Started,
                ply,
                predicted_usi: usi,
                actual_usi: None,
                deadline_at: None,
            },
            out,
        );
    }

    /// Whether an open ponder search predicted the move that just arrived.
    fn ponder_verdict(&self, csa: &str) -> Option<bool> {
        let search = self.search?;
        if search.kind != SearchKind::Ponder {
            return None;
        }
        Some(self.predicted.as_ref().is_some_and(|predicted| predicted.csa == csa))
    }

    /// The prediction held, so the running search becomes the real one.
    ///
    /// Only the clock changes: the engine is already on this position, so it
    /// keeps its tree and everything [`SearchInfo`] has collected still describes
    /// the position we are about to move in.
    fn on_ponder_hit(&mut self, at: Millis, out: &mut Vec<Action>) {
        if let Some(context) = &mut self.probe_context {
            context.ponder_outcome = Some("hit".into());
        }
        self.probe_search(at, "ponder_hit", "thinking", None, out);
        out.push(Action::SendEngine(UsiCommand::PonderHit));
        let deadline_at = self.move_deadline(at);
        if let Some(search) = &mut self.search {
            search.kind = SearchKind::Normal;
            search.deadline_at = deadline_at;
        }
        self.want_go = false;
        let predicted = self.predicted.take();
        let (ply, usi) = predicted
            .map_or_else(|| (0, String::new()), |predicted| (predicted.ply, predicted.usi));
        self.log(
            at,
            RecordBody::Ponder {
                outcome: PonderOutcome::Hit,
                ply,
                predicted_usi: usi.clone(),
                actual_usi: Some(usi),
                deadline_at,
            },
            out,
        );
    }

    /// The prediction failed, so the running search answers a position that
    /// never happened.
    ///
    /// A new `go` cannot be sent until the engine releases this one, because a
    /// `bestmove` carries no marker saying which `go` it answers. So `stop` goes
    /// out now and the real move deadline is armed on the doomed search: if the
    /// engine lets go promptly the deadline is discarded and a proper search
    /// starts, and if it never lets go the ordinary hang ladder runs at exactly
    /// the moment it would have run anyway.
    fn abandon_ponder(&mut self, at: Millis, actual_usi: &str, out: &mut Vec<Action>) {
        if let Some(context) = &mut self.probe_context {
            context.ponder_outcome = Some("miss".into());
        }
        self.probe_search(at, "discarded", "discarded", Some("ponder_miss"), out);
        let deadline_at = self.move_deadline(at);
        self.generation = self.generation.saturating_add(1);
        // The principal variation describes the other position, and the fallback
        // ladder would otherwise play from it.
        self.info = SearchInfo::default();
        if let Some(search) = &mut self.search {
            search.deadline_at = deadline_at;
            search.drain_deadline = Some(at.saturating_add(self.config.stop_grace_ms));
        }
        out.push(Action::SendEngine(UsiCommand::Stop));
        let predicted = self.predicted.take();
        let (ply, usi) = predicted
            .map_or_else(|| (0, String::new()), |predicted| (predicted.ply, predicted.usi));
        self.log(
            at,
            RecordBody::Ponder {
                outcome: PonderOutcome::Miss,
                ply,
                predicted_usi: usi,
                actual_usi: Some(actual_usi.to_owned()),
                deadline_at,
            },
            out,
        );
    }

    fn stop_search(&mut self, at: Millis, out: &mut Vec<Action>) {
        // Writing to an engine that cannot receive it is how a dead engine used
        // to take the whole bridge down with a broken pipe.
        let alive = matches!(self.engine, EnginePhase::Busy | EnginePhase::Ready);
        let mut stopped = false;
        if let Some(search) = &mut self.search {
            if search.stop_sent_at.is_none() {
                search.stop_sent_at = Some(at);
                stopped = true;
                if alive {
                    out.push(Action::SendEngine(UsiCommand::Stop));
                }
            }
        }
        if stopped
            && self.probe_context.as_ref().is_some_and(|context| context.state != "discarded")
        {
            self.probe_search(at, "stop_requested", "stopping", None, out);
        }
    }

    fn maybe_send_operator_resign(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.operator_resign != OperatorResignState::Pending {
            return;
        }
        let our_turn = self.game.as_ref().is_some_and(GameState::is_our_turn);
        if !our_turn || self.pending_origin.is_some() {
            self.stop_search(at, out);
            self.discard_search(at);
            self.probe_search(at, "discarded", "discarded", Some("operator_resign"), out);
            return;
        }

        self.want_go = false;
        self.stop_search(at, out);
        self.discard_search(at);
        self.probe_search(at, "discarded", "discarded", Some("operator_resign"), out);
        self.send_special(at, SpecialMove::Toryo, out);
        self.operator_resign = OperatorResignState::Sent;
    }

    /// Everything the engine failed to give us, in order of preference.
    ///
    /// The generation is bumped here rather than when `stop` was sent, so a late
    /// answer to the abandoned search is stale by construction while an answer
    /// that arrives inside the grace window is still honoured.
    fn fallback(&mut self, at: Millis, out: &mut Vec<Action>) {
        self.want_go = false;
        if !self.search.is_some_and(|search| {
            search.generation == self.generation
                && search.kind == SearchKind::Normal
                && self.game.as_ref().is_some_and(|game| search.root_ply == game.ply())
        }) {
            self.info = SearchInfo::default();
        }
        self.generation = self.generation.saturating_add(1);
        self.discard_search(at);
        self.probe_search(at, "discarded", "discarded", Some("fallback"), out);
        self.degraded_streak = self.degraded_streak.saturating_add(1);

        if self.degraded_streak >= self.config.max_degraded_moves {
            self.send_special(at, SpecialMove::Toryo, out);
            return;
        }

        let Some(game) = &self.game else {
            return;
        };

        if let Some(usi) = self.info.pv.first() {
            if let Ok(csa) = game.csa_for_usi(usi) {
                self.send_our_move(at, &csa, MoveOrigin::FallbackPv, out);
                return;
            }
        }
        if game.can_declare_win() {
            self.send_special(at, SpecialMove::Kachi, out);
            return;
        }
        match self.config.panic_policy {
            PanicPolicy::Resign => self.send_special(at, SpecialMove::Toryo, out),
            PanicPolicy::AnyLegal => match game.first_legal_csa() {
                Some(csa) => self.send_our_move(at, &csa, MoveOrigin::FallbackAnyLegal, out),
                None => self.send_special(at, SpecialMove::Toryo, out),
            },
        }
    }

    fn check_deadlines(&mut self, at: Millis, out: &mut Vec<Action>) {
        if self.pending_log_deadline.is_some_and(|end| at >= end) {
            self.stop_after_game = true;
            self.shutdown(at, "event log completion timed out", out);
            return;
        }
        if self.server_prepare_deadline.is_some_and(|end| at >= end) {
            self.server_prepare_deadline = None;
            self.alert(
                at,
                AlertLevel::Error,
                AlertCode::ConnectionLost,
                "connection or LOGIN preparation timed out",
                out,
            );
            self.shutdown(at, "server preparation timed out", out);
            return;
        }
        if self.engine_prepare_deadline.is_some_and(|end| at >= end) {
            self.engine_prepare_deadline = None;
            self.alert(
                at,
                AlertLevel::Error,
                AlertCode::EngineHang,
                "usiok or readyok preparation timed out",
                out,
            );
            self.on_engine_exit(at, out);
            if self.closed {
                return;
            }
        }
        if self.want_go && self.turn_deadline.is_some_and(|end| at >= end) {
            self.fallback(at, out);
        }
        if self.search.is_some_and(|search| search.drain_deadline.is_some_and(|end| at >= end)) {
            self.probe_search(at, "ended", "finished", Some("drain_timeout"), out);
            self.probe_context = None;
            self.search = None;
            self.info = SearchInfo::default();
            self.engine = EnginePhase::Dead;
            self.restart_engine(at, out);
            if self.engine == EnginePhase::Dead && self.want_go {
                self.maybe_go(at, out);
            }
        }
        if let Some(search) = self.search.filter(|search| !search.abandoned) {
            if let Some(stopped) = search.stop_sent_at {
                if self.search_fallback_deadline(search, stopped).is_some_and(|end| at >= end) {
                    if let Some(open) = &mut self.search {
                        open.abandoned = true;
                    }
                    self.alert(
                        at,
                        AlertLevel::Error,
                        AlertCode::EngineHang,
                        "no bestmove before the hard deadline",
                        out,
                    );
                    self.fallback(at, out);
                }
            } else if search.deadline_at.is_some_and(|deadline| at >= deadline) {
                self.stop_search(at, out);
                if self.turn_fallback_deadline.is_some_and(|end| at >= end) {
                    self.fallback(at, out);
                }
            }
        }

        if self.connected
            && at >= self.last_server_send_at.saturating_add(self.config.effective_keepalive_ms())
        {
            self.send_server(at, CsaCommand::KeepAlive, out);
        }
        self.maybe_register(at, out);
    }

    // ---- sending -------------------------------------------------------

    fn send_our_move(&mut self, at: Millis, csa: &str, origin: MoveOrigin, out: &mut Vec<Action>) {
        let Some(mv) = CsaMove::parse(csa) else {
            return;
        };
        let comment = self.move_comment(csa);
        self.pending_origin = Some(origin);
        self.pending_eval = Some(self.info.to_record());
        self.send_server(at, CsaCommand::Move(MoveLine { csa: mv, elapsed: None, comment }), out);
    }

    /// The `,'* <score> <pv...>` floodgate records against a move.
    ///
    /// The score is fixed to Black's perspective here; [`SearchInfo`] remains
    /// in the engine's perspective for records and decisions.
    ///
    /// The variation is spelled from the board after `csa`, so it reads as the
    /// continuation rather than repeating the move it hangs off.
    fn move_comment(&self, csa: &str) -> Option<String> {
        if !self.config.send_score_comment {
            return None;
        }
        let score = self.info.comment_score()?;
        let score = match self.game.as_ref()?.our_color() {
            Color::Black => score,
            Color::White => score.checked_neg()?,
        };
        let mut comment = format!("* {score}");
        if self.config.pv_comment_moves > 0 {
            if let Some(game) = &self.game {
                for csa_move in game.csa_pv_after(csa, &self.info.pv, self.config.pv_comment_moves)
                {
                    comment.push(' ');
                    comment.push_str(&csa_move);
                }
            }
        }
        Some(comment)
    }

    fn send_special(&mut self, at: Millis, kind: SpecialMove, out: &mut Vec<Action>) {
        self.send_server(at, CsaCommand::Special { kind, elapsed: None }, out);
    }

    fn send_server(&mut self, at: Millis, command: CsaCommand, out: &mut Vec<Action>) {
        self.last_server_send_at = at;
        out.push(Action::SendServer(command));
    }

    // ---- game lifecycle ------------------------------------------------

    fn finish_game(&mut self, at: Millis, marker: &EndMarker, out: &mut Vec<Action>) {
        self.stop_search(at, out);
        // Any answer to the search that was running is now meaningless.
        self.generation = self.generation.saturating_add(1);

        let (black_remaining_ms, white_remaining_ms) = self.game.as_ref().map_or((0, 0), |game| {
            (game.ledger().remaining_ms(Color::Black), game.ledger().remaining_ms(Color::White))
        });
        let result = match marker {
            EndMarker::Win => GameResult::Win,
            EndMarker::Lose => GameResult::Lose,
            EndMarker::Draw | EndMarker::Censored => GameResult::Draw,
            _ => GameResult::Unknown,
        };
        let terminal = std::mem::take(&mut self.terminal);
        self.log(
            at,
            RecordBody::GameEnd { result, terminal, black_remaining_ms, white_remaining_ms },
            out,
        );

        if let Some(usi_result) = usi_game_result(marker) {
            out.push(Action::SendEngine(UsiCommand::GameOver(usi_result)));
        }

        self.reset_game(at, out);
        self.games_played = self.games_played.saturating_add(1);
        self.server = ServerPhase::Waiting;
        self.log(at, RecordBody::State { phase: Phase::Idle }, out);

        if self.should_continue() {
            self.registration_pending = true;
        } else {
            self.shutdown(at, "requested games are complete", out);
        }
    }

    fn maybe_register(&mut self, at: Millis, out: &mut Vec<Action>) {
        if !self.registration_pending
            || !self.connected
            || self.server != ServerPhase::Waiting
            || !self.should_continue()
            || self.search.is_some()
            || self.engine != EnginePhase::Ready
            || self.pending_log_seq.is_some()
            || self.local_reject_pending
        {
            return;
        }
        self.registration_pending = false;
        if self.config.x1 {
            if let Some(name) = self.config.game_name.clone() {
                self.send_server(
                    at,
                    CsaCommand::Meta(MetaCommand::Game {
                        name: Some(name),
                        turn: Some(self.config.turn),
                    }),
                    out,
                );
            }
        }
    }

    fn prepare_game(&mut self, out: &mut Vec<Action>) {
        if !self.game_setup_pending || self.search.is_some() || self.engine != EnginePhase::Ready {
            return;
        }
        if let Some(rule) = self.entering_king_rule {
            out.push(Action::SendEngine(UsiCommand::SetOption {
                name: "EnteringKingRule".to_owned(),
                value: Some(usi_entering_king_rule(rule).to_owned()),
            }));
            out.push(Action::SendEngine(UsiCommand::UsiNewGame));
            self.game_setup_pending = false;
        }
    }

    fn discard_search(&mut self, at: Millis) {
        if let Some(search) = &mut self.search {
            if search.generation == self.generation {
                self.generation = self.generation.saturating_add(1);
            }
            search.abandoned = true;
            search
                .drain_deadline
                .get_or_insert_with(|| at.saturating_add(self.config.stop_grace_ms));
        }
    }

    fn reset_game(&mut self, at: Millis, out: &mut Vec<Action>) {
        self.stop_search(at, out);
        self.discard_search(at);
        if let Some(context) = &mut self.probe_context {
            if context.ponder_outcome.as_deref() == Some("pending") {
                context.ponder_outcome = Some("cancelled".into());
            }
        }
        self.probe_search(at, "discarded", "discarded", Some("game_reset"), out);
        self.reader = None;
        self.summary_failure = None;
        self.deferred_summary = None;
        self.game = None;
        self.summary = None;
        self.game_id = None;
        self.game_start_seq = 0;
        self.proposal_pending = false;
        self.local_reject_pending = false;
        self.proposal_game_id = None;
        // Whatever was running still owes a `bestmove`, which will arrive after
        // the game is gone.
        self.game_setup_pending = false;
        self.want_go = false;
        self.turn_started_at = None;
        self.turn_deadline = None;
        self.turn_fallback_deadline = None;
        self.info = SearchInfo::default();
        self.ponder_hint = None;
        self.predicted = None;
        self.pending_origin = None;
        self.pending_eval = None;
        self.operator_resign = OperatorResignState::None;
        self.terminal.clear();
        self.entering_king_rule = None;
        self.engine_restarts = 0;
    }

    const fn should_continue(&self) -> bool {
        !self.stop_after_game && (self.config.games == 0 || self.games_played < self.config.games)
    }

    fn shutdown(&mut self, at: Millis, reason: &str, out: &mut Vec<Action>) {
        if self.closed {
            return;
        }
        self.end_proposal(at, ProposalEndReason::SessionShutdown, Some(reason.to_owned()), out);
        // The protocol asks for an explicit `LOGOUT` rather than a bare
        // disconnect, but only while the connection is still ours to leave. We
        // do not wait for `LOGOUT:completed`: the shell queues this line and
        // drains its writer within a finite shutdown deadline. Local write
        // completion is not evidence of server acceptance.
        if self.connected {
            self.connected = false;
            self.send_server(at, CsaCommand::Logout, out);
        }
        self.log(at, RecordBody::State { phase: Phase::Closing }, out);
        // Last, and only on the way out under our own power. `Closing` above
        // marks the start of shutting down; a bridge killed during it leaves
        // that as its final word. This record is what separates "the log ends
        // here" from "the log stops here".
        self.log(at, RecordBody::BridgeStop, out);
        self.closed = true;
        self.server = ServerPhase::Closed;
        out.push(Action::Shutdown { reason: reason.to_owned() });
    }

    // ---- logging -------------------------------------------------------

    fn spawn_engine(&mut self, at: Millis, out: &mut Vec<Action>) {
        self.engine_spawn_generation = self.engine_spawn_generation.saturating_add(1);
        self.log_with_game_id(
            at,
            None,
            RecordBody::EngineGeneration { generation: self.engine_spawn_generation },
            out,
        );
        out.push(Action::SpawnEngine);
    }

    fn connection(&mut self, at: Millis, event: ConnectionEvent, out: &mut Vec<Action>) {
        self.log_with_game_id(at, None, RecordBody::Connection { event }, out);
    }

    fn end_proposal(
        &mut self,
        at: Millis,
        reason: ProposalEndReason,
        detail: Option<String>,
        out: &mut Vec<Action>,
    ) {
        if !self.proposal_pending && self.game.is_none() {
            return;
        }
        self.log(
            at,
            RecordBody::ProposalEnd {
                game_start_seq: (self.game_start_seq != 0).then_some(self.game_start_seq),
                proposal_game_id: self.proposal_game_id.clone(),
                reason,
                detail,
            },
            out,
        );
        self.proposal_pending = false;
    }

    fn log(&mut self, at: Millis, body: RecordBody, out: &mut Vec<Action>) {
        self.log_with_game_id(at, self.game_id.clone(), body, out);
    }

    fn log_with_game_id(
        &mut self,
        at: Millis,
        game_id: Option<String>,
        body: RecordBody,
        out: &mut Vec<Action>,
    ) {
        self.seq = self.seq.saturating_add(1);
        self.last_log_at = at;
        if self.log_completion_barrier
            && matches!(body, RecordBody::GameEnd { .. } | RecordBody::ProposalEnd { .. })
        {
            self.pending_log_seq = Some(self.seq);
            self.pending_log_deadline = Some(at.saturating_add(LOG_COMPLETION_TIMEOUT_MS));
        }
        out.push(Action::Log(LogRecord { seq: self.seq, game_id, at, body }));
    }

    fn alert(
        &mut self,
        at: Millis,
        level: AlertLevel,
        code: AlertCode,
        detail: &str,
        out: &mut Vec<Action>,
    ) {
        self.log(at, RecordBody::Alert { level, code, detail: detail.to_owned() }, out);
    }
}

/// Validate and replay a proposal before exposing any accepted-game state.
fn game_from_proposal(
    summary: &GameSummary,
    rule: EnteringKingRule,
) -> Result<(GameState, Vec<MoveRecord>), String> {
    validate_proposal(summary)?;
    let mut game = GameState::from_summary(summary, rule).map_err(|err| err.to_string())?;
    if [Color::Black, Color::White]
        .into_iter()
        .any(|color| game.ledger().budget(color) == MoveBudget::Unlimited)
    {
        return Err("time-unlimited games are not supported".to_owned());
    }
    let (_, replayed) = summary.position_split();
    let mut replayed_records = Vec::new();
    for raw in replayed {
        let Ok(CsaCommand::Move(line)) = parse_line(raw) else {
            return Err(raw.to_owned());
        };
        let played = game
            .apply_csa_move(line.csa.as_str(), line.elapsed.unwrap_or(0))
            .map_err(|err| err.to_string())?;
        replayed_records.push(move_record(&played, MoveOrigin::Replayed, None));
    }
    // `To_Move` describes the position after replay; a mismatch would make
    // every subsequent move use the wrong board.
    if let Some(declared) = summary.to_move {
        if declared != game.side_to_move() {
            return Err(format!(
                "`To_Move:{declared}` but the position block leaves `{}` to move",
                game.side_to_move()
            ));
        }
    }
    // Declared remaining time applies after replay and overrides its charges.
    game.sync_declared_remaining(summary);
    Ok((game, replayed_records))
}

fn validate_proposal(summary: &GameSummary) -> Result<(), String> {
    match summary.protocol_version.as_deref() {
        Some("1.2") => {}
        Some(version) => return Err(format!("unsupported Protocol_Version `{version}`")),
        None => return Err("missing required Protocol_Version".to_owned()),
    }
    match summary.protocol_mode.as_deref() {
        None | Some("Server") => {}
        Some(mode) => return Err(format!("unsupported Protocol_Mode `{mode}`")),
    }
    match summary.format.as_deref() {
        Some("Shogi 1.0") => {}
        Some(format) => return Err(format!("unsupported Format `{format}`")),
        None => return Err("missing required Format".to_owned()),
    }
    if summary.name_black.is_none() {
        return Err("missing required Name+".to_owned());
    }
    if summary.name_white.is_none() {
        return Err("missing required Name-".to_owned());
    }
    if summary.your_turn.is_none() {
        return Err("missing required Your_Turn".to_owned());
    }
    if summary.to_move.is_none() {
        return Err("missing required To_Move".to_owned());
    }
    validate_position_shape(summary)?;
    if summary.rematch_on_draw.as_deref().is_some_and(|value| value != "NO") {
        return Err("unsupported Rematch_On_Draw value".to_owned());
    }
    Ok(())
}

fn validate_position_shape(summary: &GameSummary) -> Result<(), String> {
    let (board, _) = summary.position_split();
    let Some((turn, setup)) = board.split_last() else {
        return Err("missing required Position block".to_owned());
    };
    if turn != "+" && turn != "-" {
        return Err("Position block must end with `+` or `-`".to_owned());
    }
    if setup.is_empty() {
        return Err("Position block has no board setup".to_owned());
    }
    for line in setup {
        let bytes = line.as_bytes();
        let recognized =
            line.starts_with("PI") || matches!(bytes, [b'P', b'1'..=b'9' | b'+' | b'-', ..]);
        if !recognized {
            return Err(format!("unsupported Position line `{line}`"));
        }
    }
    Ok(())
}

const fn merge_deadline(current: Option<Millis>, candidate: Option<Millis>) -> Option<Millis> {
    match (current, candidate) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(if a < b { a } else { b }),
    }
}

const fn record_side(color: Color) -> Side {
    match color {
        Color::Black => Side::Black,
        Color::White => Side::White,
    }
}

fn move_record(
    played: &crate::game_state::PlayedMove,
    origin: MoveOrigin,
    eval: Option<EvalRecord>,
) -> MoveRecord {
    MoveRecord {
        ply: played.ply,
        side: record_side(played.color),
        usi: played.usi.clone(),
        csa: played.csa.clone(),
        t_ms: played.charged_ms,
        black_remaining_ms: played.black_remaining_ms,
        white_remaining_ms: played.white_remaining_ms,
        eval,
        by: origin,
    }
}

const fn time_summary(game: &GameState) -> TimeSummary {
    let rules = game.ledger().rules(game.our_color());
    let unit_ms = rules.unit_ms();
    TimeSummary {
        total_ms: rules.total_time_units.saturating_mul(unit_ms),
        byoyomi_ms: rules.byoyomi_units.saturating_mul(unit_ms),
        inc_ms: rules.increment_units.saturating_mul(unit_ms),
        unit_ms,
        least_ms: rules.least_time_per_move_units.saturating_mul(unit_ms),
        roundup: rules.time_roundup,
    }
}

const fn usi_game_result(marker: &EndMarker) -> Option<rsshogi_usi::GameResult> {
    match marker {
        EndMarker::Win => Some(rsshogi_usi::GameResult::Win),
        EndMarker::Lose => Some(rsshogi_usi::GameResult::Lose),
        EndMarker::Draw | EndMarker::Censored => Some(rsshogi_usi::GameResult::Draw),
        _ => None,
    }
}

fn parse_usi(line: &str) -> Result<UsiCommand, rsshogi_usi::ParseError> {
    rsshogi_usi::parse_line(line)
}

fn parse_line_usi(line: &str) -> Result<UsiCommand, rsshogi_usi::ParseError> {
    rsshogi_usi::parse_line(line)
}

/// The entering-king rule the core will verify declarations against.
///
/// Exposed so a shell can log it before the first game.
#[must_use]
pub fn effective_entering_king_rule(
    config: &BridgeConfig,
    summary: &GameSummary,
) -> EnteringKingRule {
    config.entering_king_rule.unwrap_or_else(|| entering_king_rule_for(summary))
}

#[cfg(test)]
mod tests {
    use super::{SearchInfo, MATE_SCORE};

    fn mate(ply: i32) -> SearchInfo {
        SearchInfo { mate: Some(ply), ..SearchInfo::default() }
    }

    #[test]
    fn a_mate_score_folds_onto_the_centipawn_scale() {
        // Floodgate plots the comment as a graph, so a mate has to land on the
        // same axis as everything else or the record goes blank exactly where
        // the game was decided.
        assert_eq!(mate(1).comment_score(), Some(MATE_SCORE - 1));
        assert_eq!(mate(-2).comment_score(), Some(-(MATE_SCORE - 2)));
        assert_eq!(mate(0).comment_score(), Some(-MATE_SCORE));

        // A win or loss at an unstated distance is as strong as the scale goes.
        assert_eq!(mate(i32::MAX).comment_score(), Some(MATE_SCORE));
        assert_eq!(mate(i32::MIN).comment_score(), Some(-MATE_SCORE));

        // A distance beyond the scale must not wrap around into its opposite.
        assert_eq!(mate(MATE_SCORE + 100).comment_score(), Some(0));
        assert_eq!(mate(-(MATE_SCORE + 100)).comment_score(), Some(0));
    }

    #[test]
    fn a_centipawn_score_is_reported_as_it_stands() {
        let info = SearchInfo { cp: Some(-45), ..SearchInfo::default() };
        assert_eq!(info.comment_score(), Some(-45));
        assert_eq!(SearchInfo::default().comment_score(), None);
    }
}
