//! The seams where a game gets lost.
//!
//! These are not happy-path tests. Each one reproduces an interleaving that
//! cannot be produced by inspection of any single layer, and that would need a
//! socket and a subprocess to hit at runtime. Feeding `Event`s is the whole
//! point of the session core being sans-I/O.

use rsshogi_csa::format_command;
use rsshogi_csa_bridge::{
    Action, AlertCode, BridgeConfig, Event, GameResult, Millis, MoveOrigin, OperatorCommand, Phase,
    RecordBody, SessionCore, Side,
};

const PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+floodgate-900-0+us+them+20260804120000
Name+:us
Name-:them
Your_Turn:+
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:900
Byoyomi:0
Increment:0
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
END Position
END Game_Summary";

const WHITE_PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+floodgate-900-0+them+us+20260804120000
Name+:them
Name-:us
Your_Turn:-
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:900
Byoyomi:0
Increment:0
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
END Position
END Game_Summary";

const RESUMED_PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+floodgate-900-0+us+them+20260804130000
Name+:us
Name-:them
Your_Turn:+
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:900
Byoyomi:0
Increment:0
Least_Time_Per_Move:0
Remaining_Time+:812
Remaining_Time-:848
END Time
BEGIN Position
PI
+
+7776FU,T30
-3334FU,T22
+2726FU,T58
-4132KI,T30
END Position
END Game_Summary";

const DENRYU_ZERO_TIME_PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:dr7tsec+buoy_blackbid600_tsec7t1-1-bottom_4_tmoq_hatena-600-2F+tmoq+hatena+20260606221756
Name+:us
Name-:them
Your_Turn:+
Rematch_On_Draw:NO
To_Move:+
Max_Moves:512
BEGIN Time
Time_Unit:1sec
Total_Time:600
Byoyomi:0
Increment:2
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
+5958OU,T602
-5152OU,T2
+5859OU,T2
-5251OU,T2
END Position
END Game_Summary";

struct Harness {
    core: SessionCore,
    log: Vec<Action>,
}

impl Harness {
    fn new(config: BridgeConfig) -> Self {
        Self { core: SessionCore::new(config, "engine.exe", "test"), log: Vec::new() }
    }

    fn feed(&mut self, event: Event) -> Vec<Action> {
        let actions = self.core.step(event);
        self.log.extend(actions.iter().cloned());
        actions
    }

    fn server(&mut self, at: Millis, line: &str) -> Vec<Action> {
        self.feed(Event::ServerLine { at, line: line.to_owned() })
    }

    fn engine(&mut self, at: Millis, line: &str) -> Vec<Action> {
        self.feed(Event::EngineLine { at, line: line.to_owned() })
    }

    fn tick(&mut self, at: Millis) -> Vec<Action> {
        self.feed(Event::Tick { at })
    }

    fn summary(&mut self, at: Millis, block: &str) {
        for line in block.lines() {
            self.server(at, line);
        }
    }

    /// Brings the session to `START`, with us to move.
    fn start_game(&mut self, block: &str) {
        self.feed(Event::Started { at: 0 });
        self.engine(10, "usiok");
        self.engine(20, "readyok");
        self.feed(Event::Connected { at: 30 });
        self.server(40, "LOGIN:us OK");
        self.summary(50, block);
        self.server(60, "START:test+floodgate-900-0+us+them+20260804120000");
    }

    /// Where the log stands now, for `since` to measure from.
    const fn mark(&self) -> usize {
        self.log.len()
    }

    /// Everything emitted after `mark`, when the interesting part spans several
    /// events — a summary block arrives one line at a time.
    fn since(&self, mark: usize) -> &[Action] {
        &self.log[mark..]
    }

    fn alerts(&self) -> Vec<AlertCode> {
        self.records()
            .into_iter()
            .filter_map(|body| match body {
                RecordBody::Alert { code, .. } => Some(code),
                _ => None,
            })
            .collect()
    }

    fn records(&self) -> Vec<RecordBody> {
        self.log
            .iter()
            .filter_map(|action| match action {
                Action::Log(record) => Some(record.body.clone()),
                _ => None,
            })
            .collect()
    }
}

fn config() -> BridgeConfig {
    let mut config = BridgeConfig::floodgate("us", "floodgate-900-0,secret");
    config.games = 1;
    config
}

fn waiting_harness() -> Harness {
    let mut harness = Harness::new(forever_config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");
    harness
}

/// Keeps playing after a game ends, so a straggler still has somewhere to land.
fn forever_config() -> BridgeConfig {
    let mut config = config();
    config.games = 0;
    config
}

/// Pushes the keepalive far out so `next_deadline` reports the search deadline.
///
/// With a 900 second game and a 40 second keepalive, the keepalive always comes
/// first, which is correct in production and useless when the point of the test
/// is the search deadline.
fn hang_config() -> BridgeConfig {
    let mut config = config();
    config.keepalive_interval_ms = 3_600_000;
    config
}

fn server_lines(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::SendServer(command) => Some(format_command(command)),
            _ => None,
        })
        .collect()
}

fn engine_lines(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::SendEngine(command) => Some(rsshogi_usi::format_command(command)),
            _ => None,
        })
        .collect()
}

/// A search started on our own clock. `go ponder` is deliberately not one.
fn go_line(actions: &[Action]) -> Option<String> {
    engine_lines(actions)
        .into_iter()
        .find(|line| line.starts_with("go") && !line.starts_with("go ponder"))
}

fn ponder_go_line(actions: &[Action]) -> Option<String> {
    engine_lines(actions).into_iter().find(|line| line.starts_with("go ponder"))
}

fn position_line(actions: &[Action]) -> Option<String> {
    engine_lines(actions).into_iter().find(|line| line.starts_with("position"))
}

// ---------------------------------------------------------------------------
// Seam 1: the terminal burst arrives instead of our own move's echo.
// ---------------------------------------------------------------------------

#[test]
fn terminal_burst_can_replace_our_move_echo() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    let sent = harness.engine(100, "bestmove 7g7f");
    assert_eq!(server_lines(&sent), ["+7776FU"]);

    // No echo comes back. The server declares the game lost on time instead.
    let a = harness.server(200, "#TIME_UP");
    assert!(server_lines(&a).is_empty(), "a reason line alone must not trigger anything");
    let b = harness.server(200, "#LOSE");

    let ended = harness.records().into_iter().find_map(|body| match body {
        RecordBody::GameEnd { result, terminal, .. } => Some((result, terminal)),
        _ => None,
    });
    let (result, terminal) = ended.expect("the game should have ended");
    assert_eq!(result, GameResult::Lose);
    assert_eq!(terminal, ["#TIME_UP".to_owned(), "#LOSE".to_owned()]);

    assert!(engine_lines(&b).iter().any(|line| line == "gameover lose"));
    // `game.rb` ends the connection along with the game for a plain-CSA player,
    // so a `LOGOUT` here would only race the teardown.
    assert!(server_lines(&b).is_empty(), "nothing may be sent after the game is over");
    assert!(b.iter().any(|action| matches!(action, Action::Shutdown { .. })));
}

#[test]
fn a_repetition_draw_burst_is_read_as_a_draw() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f");

    harness.server(200, "#SENNICHITE");
    let end = harness.server(200, "#DRAW");

    let ended = harness.records().into_iter().find_map(|body| match body {
        RecordBody::GameEnd { result, terminal, .. } => Some((result, terminal)),
        _ => None,
    });
    let (result, terminal) = ended.expect("the game should have ended");
    assert_eq!(result, GameResult::Draw);
    assert_eq!(terminal, ["#SENNICHITE".to_owned(), "#DRAW".to_owned()]);
    assert!(engine_lines(&end).iter().any(|line| line == "gameover draw"));
}

#[test]
fn a_late_time_up_during_a_search_discards_the_eventual_answer() {
    let mut harness = Harness::new(forever_config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f");
    harness.server(200, "+7776FU,T3");
    let opponent = harness.server(300, "-3334FU,T4");
    assert!(go_line(&opponent).is_some(), "our turn again, so a search starts");

    // The server declares us lost while the engine is still thinking.
    harness.server(400, "#TIME_UP");
    let end = harness.server(400, "#LOSE");
    assert!(engine_lines(&end).iter().any(|line| line == "stop"));

    // The search answers anyway. It must not reach the socket, and it is not an
    // anomaly either: we dropped that search ourselves and knew the answer was
    // still coming.
    let straggler = harness.engine(500, "bestmove 2g2f");
    assert!(server_lines(&straggler).is_empty());
    assert!(!harness.alerts().contains(&AlertCode::StaleBestmove));
}

// ---------------------------------------------------------------------------
// Seam 2: one read delivers our echo, the reply, and a keepalive together.
// ---------------------------------------------------------------------------

#[test]
fn a_batched_read_applies_the_echo_before_the_reply() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f");

    // All three arrive in the same read, so they carry the same timestamp.
    let echo = harness.server(1_000, "+7776FU,T12");
    assert!(go_line(&echo).is_none(), "our own echo hands the turn to the opponent");

    let reply = harness.server(1_000, "-3334FU,T5");
    let keepalive = harness.server(1_000, "");

    let go = go_line(&reply).expect("the reply gives us the move");
    assert!(go_line(&keepalive).is_none(), "exactly one search per turn");

    // 900 - 12 = 888 seconds for us, minus the 2500 ms margin.
    // 900 - 5 = 895 seconds for the opponent, reported exactly.
    assert_eq!(go, "go btime 885500 wtime 895000");

    let moves: Vec<_> = harness
        .records()
        .into_iter()
        .filter_map(|body| match body {
            RecordBody::Move(record) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(moves.len(), 2);
    assert_eq!(moves[0].usi, "7g7f");
    assert_eq!(moves[0].by, MoveOrigin::Engine);
    assert_eq!(moves[0].black_remaining_ms, 888_000);
    assert_eq!(moves[1].usi, "3c3d");
    assert_eq!(moves[1].by, MoveOrigin::Opponent);
    assert_eq!(moves[1].white_remaining_ms, 895_000);
}

// ---------------------------------------------------------------------------
// Seam 3: the engine goes silent, the ladder runs, and the answer arrives late.
// ---------------------------------------------------------------------------

#[test]
fn a_silent_engine_walks_the_ladder_and_its_late_answer_is_discarded() {
    let mut harness = Harness::new(hang_config());
    harness.start_game(PROPOSAL);

    // A principal variation lands early, then the engine stops answering.
    harness.engine(100, "info depth 12 score cp 45 nodes 900000 pv 7g7f 3c3d 2g2f");

    let deadline = harness.core.next_deadline().expect("a search deadline is armed");
    let stopping = harness.tick(deadline);
    assert!(engine_lines(&stopping).iter().any(|line| line == "stop"));
    assert!(server_lines(&stopping).is_empty(), "the grace window has not run out yet");

    let hard = harness.core.next_deadline().expect("the grace deadline is armed");
    let laddered = harness.tick(hard);
    assert_eq!(server_lines(&laddered), ["+7776FU,'* 45 -3334FU +2726FU"], "the pv move is played");
    assert!(harness.alerts().contains(&AlertCode::EngineHang));

    // Firing once is enough: the grace deadline stays in the past forever.
    let quiet = harness.tick(hard + 5_000);
    assert!(server_lines(&quiet).is_empty(), "the hang must not be reported again");

    // The abandoned search finally answers.
    let straggler = harness.engine(hard + 6_000, "bestmove 8h2b+");
    assert!(server_lines(&straggler).is_empty());
    assert!(harness.alerts().contains(&AlertCode::StaleBestmove));
}

#[test]
fn a_second_consecutive_hang_resigns() {
    let mut harness = Harness::new(hang_config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 12 score cp 45 pv 7g7f 3c3d");

    let deadline = harness.core.next_deadline().expect("armed");
    harness.tick(deadline);
    let hard = harness.core.next_deadline().expect("armed");
    let first = harness.tick(hard);
    assert_eq!(server_lines(&first), ["+7776FU,'* 45 -3334FU"]);

    // The engine returns, the game moves on, and the next search hangs with no pv.
    harness.engine(hard + 100, "bestmove 7g7f");
    harness.server(hard + 200, "+7776FU,T2");
    let reply = harness.server(hard + 300, "-3334FU,T2");
    assert!(go_line(&reply).is_some());

    let deadline = harness.core.next_deadline().expect("armed");
    harness.tick(deadline);
    let hard = harness.core.next_deadline().expect("armed");
    let second = harness.tick(hard);
    assert_eq!(server_lines(&second), ["%TORYO"], "two failures in a row is a resignation");
}

// ---------------------------------------------------------------------------
// Seam 4: telling a real resignation from a wedged GPU.
// ---------------------------------------------------------------------------

#[test]
fn a_resign_without_a_score_is_passed_on_and_reported() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    harness.engine(100, "info depth 3 pv 7g7f");
    let answered = harness.engine(200, "bestmove resign");

    // Giving up is the engine's decision to make, however little it explained
    // itself. What the bridge owes is a log that says so.
    assert_eq!(server_lines(&answered), ["%TORYO"]);
    assert!(harness.alerts().contains(&AlertCode::SuspectResign));
}

#[test]
fn a_resign_backed_by_a_losing_score_is_honoured() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    harness.engine(100, "info depth 30 score mate -5 pv 7g7f");
    let answered = harness.engine(200, "bestmove resign");

    assert_eq!(server_lines(&answered), ["%TORYO"]);
    assert!(!harness.alerts().contains(&AlertCode::SuspectResign));
}

#[test]
fn a_resign_at_a_merely_bad_score_is_the_engines_call() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    // When to give up is the engine's own setting, not ours to second-guess.
    harness.engine(100, "info depth 20 score cp -500 pv 7g7f");
    let answered = harness.engine(200, "bestmove resign");

    assert_eq!(server_lines(&answered), ["%TORYO"]);
    assert!(!harness.alerts().contains(&AlertCode::SuspectResign));
}

#[test]
fn a_resign_from_a_position_the_engine_called_won_is_passed_on_and_reported() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    harness.engine(100, "info depth 20 score cp 500 pv 7g7f 3c3d");
    let answered = harness.engine(200, "bestmove resign");

    // Odd enough to be worth an error in the log, not odd enough to override.
    assert_eq!(server_lines(&answered), ["%TORYO"]);
    assert!(harness.alerts().contains(&AlertCode::SuspectResign));
}

#[test]
fn a_mate_score_still_reaches_the_record() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    // Measured on floodgate: the last few moves of a decisive game carry a mate
    // score, and a comment keyed on centipawns alone drops them entirely.
    harness.engine(100, "info depth 12 score mate 3 pv 7g7f 3c3d");
    let answered = harness.engine(200, "bestmove 7g7f");

    assert_eq!(server_lines(&answered), ["+7776FU,'* 31997 -3334FU"]);
}

#[test]
fn a_black_centipawn_comment_keeps_the_engine_score() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    harness.engine(100, "info score cp -30");
    let answered = harness.engine(200, "bestmove 7g7f");

    assert_eq!(server_lines(&answered), ["+7776FU,'* -30"]);
}

#[test]
fn white_centipawn_comments_use_the_fixed_black_perspective() {
    for (engine_score, csa_score) in [(-30, 30), (45, -45)] {
        let mut harness = Harness::new(config());
        harness.start_game(WHITE_PROPOSAL);
        harness.server(80, "+7776FU,T1");

        harness.engine(100, &format!("info score cp {engine_score}"));
        let answered = harness.engine(200, "bestmove 3c3d");

        assert_eq!(server_lines(&answered), [format!("-3334FU,'* {csa_score}")]);
    }
}

#[test]
fn white_mate_comments_use_the_fixed_black_perspective() {
    for (engine_score, csa_score) in [(1, -31_999), (-2, 31_998)] {
        let mut harness = Harness::new(config());
        harness.start_game(WHITE_PROPOSAL);
        harness.server(80, "+7776FU,T1");

        harness.engine(100, &format!("info score mate {engine_score}"));
        let answered = harness.engine(200, "bestmove 3c3d");

        assert_eq!(server_lines(&answered), [format!("-3334FU,'* {csa_score}")]);
    }
}

#[test]
fn white_eval_records_and_resign_checks_keep_the_engine_perspective() {
    let mut harness = Harness::new(config());
    harness.start_game(WHITE_PROPOSAL);
    harness.server(80, "+7776FU,T1");
    harness.engine(100, "info score cp -500");

    let answered = harness.engine(200, "bestmove 3c3d");
    assert_eq!(server_lines(&answered), ["-3334FU,'* 500"]);
    harness.server(300, "-3334FU,T1");

    let eval = harness.records().into_iter().find_map(|body| match body {
        RecordBody::Move(record) if record.side == Side::White => record.eval,
        _ => None,
    });
    assert_eq!(eval.expect("white move should carry its engine eval").cp, Some(-500));

    let mut resigning = Harness::new(config());
    resigning.start_game(WHITE_PROPOSAL);
    resigning.server(80, "+7776FU,T1");
    resigning.engine(100, "info score cp -500");
    let resigned = resigning.engine(200, "bestmove resign");

    assert_eq!(server_lines(&resigned), ["%TORYO"]);
    assert!(!resigning.alerts().contains(&AlertCode::SuspectResign));
}

#[test]
fn an_illegal_bestmove_never_reaches_the_socket() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    harness.engine(100, "info depth 8 score cp 20 pv 7g7f");
    let answered = harness.engine(200, "bestmove 7g7e");

    assert_eq!(server_lines(&answered), ["+7776FU,'* 20"], "the pv covers for it");
    assert!(harness.alerts().contains(&AlertCode::IllegalBestmove));
}

#[test]
fn a_declaration_we_cannot_verify_is_still_the_engines_to_make() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    // Nobody can declare from the opening, so our check disagrees. Overruling
    // it would resign a game the engine believes it has won, and our checker is
    // the same code the engine is built on — a disagreement is as likely ours.
    harness.engine(100, "info depth 8 score cp 20 pv 7g7f");
    let answered = harness.engine(200, "bestmove win");

    assert_eq!(server_lines(&answered), ["%KACHI"]);
    assert!(harness.alerts().contains(&AlertCode::UnverifiedDeclaration));
}

// ---------------------------------------------------------------------------
// Seam 5: an operator restarts the game, and an idle wait needs a keepalive.
// ---------------------------------------------------------------------------

#[test]
fn a_resumed_game_replays_into_the_right_clock_and_turn() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");
    harness.summary(50, RESUMED_PROPOSAL);
    let started = harness.server(60, "START:test+floodgate-900-0+us+them+20260804130000");

    let start_record = harness
        .records()
        .into_iter()
        .find_map(|body| match body {
            RecordBody::GameStart { moves_so_far, my_color, .. } => Some((moves_so_far, my_color)),
            _ => None,
        })
        .expect("a game should have started");
    assert_eq!(start_record.1, Side::Black);
    assert_eq!(start_record.0.len(), 4);
    assert!(start_record.0.iter().all(|record| record.by == MoveOrigin::Replayed));

    // Four moves replayed, so it is our turn again immediately.
    let go = go_line(&started).expect("a search starts as soon as the game does");
    // `Remaining_Time` wins over `Total_Time`: 812 and 848 seconds, less the margin.
    assert_eq!(go, "go btime 809500 wtime 848000");

    let position = engine_lines(&started)
        .into_iter()
        .find(|line| line.starts_with("position"))
        .expect("the engine is told the position");
    assert!(position.ends_with(" moves 7g7f 3c3d 2g2f 4a3b"), "{position}");
}

#[test]
fn denryu_zero_time_buoy_replays_t602_and_starts_with_the_increment() {
    let mut config = config();
    config.margin_ms = 500;
    let mut harness = Harness::new(config);
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");
    harness.summary(50, DENRYU_ZERO_TIME_PROPOSAL);
    let started = harness.server(
        60,
        "START:dr7tsec+buoy_blackbid600_tsec7t1-1-bottom_4_tmoq_hatena-600-2F+tmoq+hatena+20260606221756",
    );

    let moves = harness
        .records()
        .into_iter()
        .find_map(|body| match body {
            RecordBody::GameStart { moves_so_far, .. } => Some(moves_so_far),
            _ => None,
        })
        .expect("the proposal should be recorded");
    assert_eq!(moves.len(), 4);
    assert_eq!(moves[0].t_ms, 602_000);
    assert_eq!(moves[0].black_remaining_ms, 0);
    assert_eq!(moves[0].white_remaining_ms, 600_000);
    assert_eq!(moves[3].black_remaining_ms, 0);
    assert_eq!(moves[3].white_remaining_ms, 600_000);

    assert_eq!(go_line(&started).as_deref(), Some("go btime 0 wtime 600000 binc 1500 winc 2000"));
    let deadline = harness
        .records()
        .into_iter()
        .find_map(|body| match body {
            RecordBody::Go { deadline_at, .. } => Some(deadline_at),
            _ => None,
        })
        .expect("our turn should start a search");
    assert_eq!(deadline, 1_559);
    assert!(!harness.alerts().contains(&AlertCode::MarginExhaustsBudget));
}

#[test]
fn denryu_zero_time_buoy_warns_when_the_default_margin_consumes_the_increment() {
    let mut harness = waiting_harness();
    harness.summary(50, DENRYU_ZERO_TIME_PROPOSAL);

    assert!(harness.alerts().contains(&AlertCode::MarginExhaustsBudget));
}

#[test]
fn an_idle_wait_sends_exactly_one_keepalive_per_interval() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    let logged_in = harness.server(40, "LOGIN:us OK");
    assert!(server_lines(&logged_in).is_empty(), "csa mode does not send %%GAME");

    let due = harness.core.next_deadline().expect("a keepalive is scheduled");
    assert_eq!(due, 30 + 40_000, "the interval runs from LOGIN, the last line we sent");

    let early = harness.tick(due - 1);
    assert!(server_lines(&early).is_empty());

    let fired = harness.tick(due);
    assert_eq!(server_lines(&fired), [""], "a blank line is the keepalive");

    // The next one is a full interval later, never sooner than the 30 second rule.
    let next = harness.core.next_deadline().expect("another keepalive is scheduled");
    assert_eq!(next, due + 40_000);
    let quiet = harness.tick(due + 1_000);
    assert!(server_lines(&quiet).is_empty());
}

// ---------------------------------------------------------------------------
// Ordering and lifecycle invariants that the seams depend on.
// ---------------------------------------------------------------------------

#[test]
fn the_engine_is_warmed_before_the_socket_is_opened() {
    let mut harness = Harness::new(config());
    let started = harness.feed(Event::Started { at: 0 });
    assert!(started.contains(&Action::SpawnEngine));
    assert_eq!(engine_lines(&started), ["usi"]);
    assert!(!started.iter().any(|action| matches!(action, Action::Connect)));

    let configured = harness.engine(10, "usiok");
    assert_eq!(engine_lines(&configured), ["isready"]);
    assert!(!configured.iter().any(|action| matches!(action, Action::Connect)));

    let ready = harness.engine(20, "readyok");
    assert!(
        ready.iter().any(|action| matches!(action, Action::Connect)),
        "connecting only after readyok is what keeps the 120 second agree window free"
    );
}

#[test]
fn the_summary_sets_the_engine_rule_before_agreeing() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    let before = harness.log.len();
    harness.summary(50, PROPOSAL);
    let agreeing = &harness.log[before..];

    assert!(engine_lines(agreeing)
        .contains(&"setoption name EnteringKingRule value CSARule27".to_owned()));
    assert!(engine_lines(agreeing).contains(&"usinewgame".to_owned()));
    assert_eq!(server_lines(agreeing), ["AGREE test+floodgate-900-0+us+them+20260804120000"]);
}

#[test]
fn losing_the_connection_mid_game_is_reported_and_final() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f");

    let dropped = harness.feed(Event::ServerEof { at: 200 });
    assert!(harness.alerts().contains(&AlertCode::ConnectionLost));
    assert!(
        dropped.iter().any(|action| matches!(action, Action::Shutdown { .. })),
        "a mid-game disconnect is a loss, never a retry"
    );
    assert!(!dropped.iter().any(|action| matches!(action, Action::Connect)));
}

#[test]
fn the_operator_can_resign_the_running_game() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    let resigned = harness.feed(Event::Operator { at: 100, command: OperatorCommand::Resign });
    assert_eq!(server_lines(&resigned), ["%TORYO"]);
    assert!(engine_lines(&resigned).iter().any(|line| line == "stop"));

    let stopped = harness.engine(110, "bestmove 7g7f");
    assert!(server_lines(&stopped).is_empty(), "the stopped search cannot answer after TORYO");
}

#[test]
fn operator_resign_waits_for_our_turn_after_a_move_is_sent() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    let sent = harness.engine(100, "bestmove 7g7f");
    assert_eq!(server_lines(&sent), ["+7776FU"]);

    let deferred = harness.feed(Event::Operator { at: 110, command: OperatorCommand::Resign });
    let duplicate = harness.feed(Event::Operator { at: 120, command: OperatorCommand::Resign });
    assert!(server_lines(&deferred).is_empty());
    assert!(server_lines(&duplicate).is_empty());

    let echoed = harness.server(130, "+7776FU,T1");
    assert!(server_lines(&echoed).is_empty(), "it is now the opponent's turn");

    let resigned = harness.server(140, "-3334FU,T1");
    assert_eq!(server_lines(&resigned), ["%TORYO"]);
    assert!(go_line(&resigned).is_none(), "a pending resignation must supersede search");

    let repeated = harness.feed(Event::Operator { at: 150, command: OperatorCommand::Resign });
    assert!(server_lines(&repeated).is_empty(), "TORYO is sent only once");
}

#[test]
fn operator_resign_discards_ponder_without_running_the_fallback_ladder() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T1");

    let requested = harness.feed(Event::Operator { at: 210, command: OperatorCommand::Resign });
    assert!(server_lines(&requested).is_empty(), "we cannot resign on the opponent's turn");
    assert!(engine_lines(&requested).iter().any(|line| line == "stop"));

    let late = harness.tick(2_000);
    assert!(server_lines(&late).is_empty(), "a discarded ponder cannot run the move ladder");
    assert!(!harness.alerts().contains(&AlertCode::EngineHang));

    let released = harness.engine(2_100, "bestmove 2g2f");
    assert!(server_lines(&released).is_empty());
    let resigned = harness.server(2_200, "-3334FU,T1");
    assert_eq!(server_lines(&resigned), ["%TORYO"]);
}

// ---------------------------------------------------------------------------
// Seam 6: the engine process dies mid-game.
//
// This is not hypothetical. On 2026-08-04, in a real floodgate game against
// Owlet_v1, an unrelated job on the machine killed the engine at move 78 while
// we were winning by +9878. The ladder played the engine's last principal
// variation move and the game carried on — and then the bridge sat doing
// nothing until the clock ran out, because there was no path back to a move.
// ---------------------------------------------------------------------------

#[test]
fn an_engine_that_dies_mid_search_is_answered_from_its_own_pv() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 16 score cp 9878 pv 7g7f 3c3d");

    let died = harness.feed(Event::EngineExited { at: 200 });

    assert_eq!(server_lines(&died), ["+7776FU,'* 9878 -3334FU"], "the pv covers the move we owe");
    assert!(harness.alerts().contains(&AlertCode::EngineDead));
}

#[test]
fn a_dead_engine_is_restarted_rather_than_left_dead() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 16 score cp 9878 pv 7g7f 3c3d");

    let died = harness.feed(Event::EngineExited { at: 200 });
    assert!(died.contains(&Action::SpawnEngine), "a replacement is started");
    assert!(engine_lines(&died).contains(&"usi".to_owned()));
    assert!(harness.alerts().contains(&AlertCode::EngineRestarted));

    // The replacement is configured exactly like the first one, including the
    // rule that came from the proposal rather than from the static options.
    let handshaken = harness.engine(300, "usiok");
    let lines = engine_lines(&handshaken);
    assert!(
        lines.contains(&"setoption name EnteringKingRule value CSARule27".to_owned()),
        "{lines:?}"
    );
    assert!(lines.contains(&"isready".to_owned()));
    assert!(!lines.contains(&"usinewgame".to_owned()), "the game is already in progress");

    // Once it is ready the game continues normally.
    harness.server(400, "+7776FU,T3");
    let reply = harness.server(500, "-3334FU,T4");
    assert!(go_line(&reply).is_none(), "the replacement is not ready yet");

    let ready = harness.engine(600, "readyok");
    assert!(go_line(&ready).is_some(), "the owed move is issued as soon as it can be");
}

#[test]
fn a_dead_engine_with_no_restarts_left_resigns_instead_of_burning_the_clock() {
    let mut config = config();
    config.max_engine_restarts = 0;
    let mut harness = Harness::new(config);
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 16 score cp 9878 pv 7g7f 3c3d");

    let died = harness.feed(Event::EngineExited { at: 200 });
    assert_eq!(server_lines(&died), ["+7776FU,'* 9878 -3334FU"]);
    assert!(!died.contains(&Action::SpawnEngine));

    // The move is echoed and the opponent replies, so it is our turn again with
    // nothing to think with.
    harness.server(300, "+7776FU,T3");
    let owed = harness.server(400, "-3334FU,T4");

    assert_eq!(
        server_lines(&owed),
        ["%TORYO"],
        "sitting here would lose on time several minutes later instead"
    );
    // And it resigns once, not on every subsequent wakeup.
    let later = harness.tick(500);
    assert!(server_lines(&later).is_empty());
}

#[test]
fn an_engine_that_dies_on_the_opponents_clock_owes_nothing() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 16 score cp 9878 pv 7g7f 3c3d");
    harness.engine(150, "bestmove 7g7f");
    harness.server(200, "+7776FU,T3");

    // It is the opponent's move. Nothing is owed, so nothing may be sent.
    let died = harness.feed(Event::EngineExited { at: 300 });
    assert!(
        server_lines(&died).is_empty(),
        "resigning here would throw away a game we are still playing: {:?}",
        server_lines(&died)
    );
    assert!(died.contains(&Action::SpawnEngine));
}

#[test]
fn a_repeated_death_report_is_not_counted_twice() {
    // With no restarts allowed the engine stays dead, which is the state the
    // core can recognise on its own. Deduplicating a death report that arrives
    // *during* a restart needs the spawn generation, which only the shell knows,
    // so that half of the filter lives there.
    let mut config = config();
    config.max_engine_restarts = 0;
    let mut harness = Harness::new(config);
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 16 score cp 9878 pv 7g7f 3c3d");

    // A failed write and the reader task can both report the same death.
    let first = harness.feed(Event::EngineExited { at: 200 });
    let second = harness.feed(Event::EngineExited { at: 201 });

    assert_eq!(server_lines(&first), ["+7776FU,'* 9878 -3334FU"]);
    assert!(server_lines(&second).is_empty(), "the second report must do nothing");
    assert_eq!(
        harness.alerts().iter().filter(|code| **code == AlertCode::EngineDead).count(),
        1,
        "and must not be reported twice"
    );
}

#[test]
fn nothing_is_written_to_an_engine_that_cannot_receive_it() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "info depth 16 score cp 9878 pv 7g7f 3c3d");
    harness.feed(Event::EngineExited { at: 200 });

    // Everything from here on must avoid the dead process. Writing to it is how
    // a broken pipe used to take the whole bridge down with it.
    let mut wrote_to_dead_engine = false;
    for at in [300, 400, 500] {
        let actions = harness.tick(at);
        if engine_lines(&actions).iter().any(|line| line == "stop") {
            wrote_to_dead_engine = true;
        }
    }
    assert!(!wrote_to_dead_engine, "`stop` must not be sent to a dead engine");
}

#[test]
fn an_engine_that_dies_between_games_is_restarted_not_abandoned() {
    let mut harness = Harness::new(forever_config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    // Waiting to be paired. The engine dies with nothing at stake.
    let died = harness.feed(Event::EngineExited { at: 100 });

    assert!(died.contains(&Action::SpawnEngine), "a replacement is started");
    assert!(
        !died.iter().any(|action| matches!(action, Action::Shutdown { .. })),
        "giving up the session here means never playing at all"
    );
    assert!(harness.alerts().contains(&AlertCode::EngineRestarted));

    // And the replacement comes up without disturbing the connection.
    let handshaken = harness.engine(200, "usiok");
    assert!(engine_lines(&handshaken).contains(&"isready".to_owned()));
    let ready = harness.engine(300, "readyok");
    assert!(
        !ready.iter().any(|action| matches!(action, Action::Connect)),
        "we are already logged in"
    );
}

#[test]
fn an_engine_that_cannot_be_restarted_between_games_shuts_down() {
    let mut config = forever_config();
    config.max_engine_restarts = 0;
    let mut harness = Harness::new(config);
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    let died = harness.feed(Event::EngineExited { at: 100 });
    assert!(died.iter().any(|action| matches!(action, Action::Shutdown { .. })));
}

#[test]
fn an_engine_that_dies_during_warmup_is_restarted_and_still_connects() {
    let mut harness = Harness::new(config());
    let started = harness.feed(Event::Started { at: 0 });
    assert!(started.contains(&Action::SpawnEngine));

    // It never reaches readyok.
    let died = harness.feed(Event::EngineExited { at: 50 });
    assert!(died.contains(&Action::SpawnEngine), "try again before giving up");

    harness.engine(100, "usiok");
    let ready = harness.engine(200, "readyok");
    assert!(
        ready.iter().any(|action| matches!(action, Action::Connect)),
        "the replacement still opens the socket, in the right order"
    );
}

// ---------------------------------------------------------------------------
// Seam 8: a search is in flight during the opponent's turn.
//
// Pondering puts a search into the one window that used to be empty, and every
// way it can go wrong is paid for out of our own clock: an answer for a position
// that never happened, an answer played on the opponent's turn, or an engine
// that will not let go of the search we no longer want.
// ---------------------------------------------------------------------------

#[test]
fn a_ponder_waits_for_our_move_to_be_echoed() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    // The board only advances on the echo, and the ponder position is the board
    // after both moves, so nothing can start yet.
    let sent = harness.engine(100, "bestmove 7g7f ponder 3c3d");
    assert_eq!(server_lines(&sent), ["+7776FU"]);
    assert!(ponder_go_line(&sent).is_none(), "the board is not there yet");

    let echo = harness.server(200, "+7776FU,T3");
    let position = position_line(&echo).expect("the ponder position is set");
    assert!(position.ends_with(" moves 7g7f 3c3d"), "{position}");
    assert!(ponder_go_line(&echo).is_some(), "the search starts on the opponent's clock");
    assert!(go_line(&echo).is_none(), "it is not our clock that is running");
}

#[test]
fn a_predicted_move_becomes_the_search_we_were_already_running() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T3");
    harness.engine(250, "info depth 20 score cp 60 pv 2g2f 8c8d");

    // Exactly what the ponder assumed.
    let hit = harness.server(300, "-3334FU,T4");
    assert_eq!(engine_lines(&hit), ["ponderhit"], "no position, no new go: it is the same search");

    // Its answer is our move, carrying the evaluation the ponder built up.
    let played = harness.engine(400, "bestmove 2g2f");
    assert_eq!(server_lines(&played), ["+2726FU,'* 60 -8384FU"]);

    let ponders = harness
        .records()
        .into_iter()
        .filter(|body| matches!(body, RecordBody::Ponder { .. }))
        .count();
    assert_eq!(ponders, 2, "one start and one hit");
}

#[test]
fn a_hit_arms_the_deadline_a_normal_search_would_have_had() {
    // Otherwise the keepalive is always the nearer deadline.
    let mut harness = Harness::new(hang_config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T3");
    let idle = harness.core.next_deadline().expect("the keepalive is always armed");
    assert!(idle > 1_000_000, "only the distant keepalive: nothing is due on their clock");

    harness.server(300, "-3334FU,T4");
    let deadline = harness.core.next_deadline().expect("our clock is running now");
    assert!(deadline > 300 && deadline < idle, "the search deadline is now the nearer one");

    // From here it behaves like any other search deadline.
    let stopping = harness.tick(deadline);
    assert!(engine_lines(&stopping).iter().any(|line| line == "stop"));
}

#[test]
fn a_wrong_prediction_is_released_before_the_real_search_starts() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T3");
    harness.engine(250, "info depth 20 score cp 60 pv 2g2f 8c8d");

    // They played something else.
    let miss = harness.server(300, "-8384FU,T4");
    assert!(engine_lines(&miss).iter().any(|line| line == "stop"));
    assert!(
        go_line(&miss).is_none(),
        "a bestmove says nothing about which go it answers, so only one may be outstanding"
    );

    // The answer to the position that never happened must not be played.
    let straggler = harness.engine(350, "bestmove 2g2f");
    assert!(
        server_lines(&straggler).is_empty(),
        "that answer assumed a move the opponent did not make"
    );
    assert!(!harness.alerts().contains(&AlertCode::StaleBestmove), "a miss is not an anomaly");

    // Only now does the real search start, on the real position.
    let position = position_line(&straggler).expect("the real position is set");
    assert!(position.ends_with(" moves 7g7f 8c8d"), "{position}");
    assert!(go_line(&straggler).is_some());
}

#[test]
fn a_miss_the_engine_never_releases_still_runs_the_ladder_on_time() {
    let mut harness = Harness::new(hang_config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T3");
    harness.engine(250, "info depth 20 score cp 60 pv 2g2f 8c8d");
    harness.server(300, "-8384FU,T4");

    // The engine ignored the stop. Our own clock is what bounds the wait — not a
    // grace window, which would resign over a slow release, and not forever.
    let deadline = harness.core.next_deadline().expect("our own clock bounds this");
    assert!(deadline > 300);
    let stopping = harness.tick(deadline);
    assert!(engine_lines(&stopping).iter().any(|line| line == "stop"));

    let hard = harness.core.next_deadline().expect("the grace deadline is armed");
    let laddered = harness.tick(hard);
    assert!(harness.alerts().contains(&AlertCode::EngineHang));
    assert_eq!(
        server_lines(&laddered),
        ["%TORYO"],
        "the pv belonged to the other position, so there is nothing to fall back to"
    );
}

#[test]
fn a_prediction_the_board_refuses_is_not_pondered_on() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    // After 7g7f it is white to move, so this is nobody's move.
    harness.engine(100, "bestmove 7g7f ponder 2g2f");
    let echo = harness.server(200, "+7776FU,T3");

    assert!(ponder_go_line(&echo).is_none());
    assert!(harness.alerts().contains(&AlertCode::UnusablePonder));

    // The game carries on as if nothing had been offered.
    let reply = harness.server(300, "-3334FU,T4");
    assert!(go_line(&reply).is_some());
}

#[test]
fn a_ponder_is_never_played_as_a_move() {
    let mut harness = Harness::new(forever_config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T3");

    // The opponent resigns while we are pondering.
    harness.server(300, "%TORYO");
    harness.server(300, "#RESIGN");
    let ended = harness.server(300, "#WIN");
    assert!(engine_lines(&ended).iter().any(|line| line == "stop"));

    let straggler = harness.engine(400, "bestmove 2g2f");
    assert!(server_lines(&straggler).is_empty(), "the game is over and it was never our answer");
    assert!(!harness.alerts().contains(&AlertCode::StaleBestmove));
}

#[test]
fn an_engine_that_dies_while_pondering_owes_nothing() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    harness.server(200, "+7776FU,T3");
    harness.engine(250, "info depth 20 score cp 60 pv 2g2f 8c8d");

    let died = harness.feed(Event::EngineExited { at: 300 });
    assert!(
        server_lines(&died).is_empty(),
        "it is the opponent's turn, so the ladder would resign a game we are still playing"
    );
    assert!(died.contains(&Action::SpawnEngine));

    // The replacement comes up and answers the move we are actually owed.
    harness.engine(400, "usiok");
    harness.engine(500, "readyok");
    let reply = harness.server(600, "-3334FU,T4");
    assert!(go_line(&reply).is_some());
}

// ---------------------------------------------------------------------------
// Seam 9: the proposal describes a game we would misplay.
// ---------------------------------------------------------------------------

#[test]
fn a_proposal_whose_to_move_disagrees_with_its_position_is_refused() {
    let mut harness = Harness::new(forever_config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    // Four moves are already in, so black is to move. The proposal claims white.
    let contradictory = RESUMED_PROPOSAL.replace("To_Move:+", "To_Move:-");
    let mark = harness.mark();
    harness.summary(50, &contradictory);

    let sent = server_lines(harness.since(mark));
    assert!(
        sent.iter().any(|line| line.starts_with("REJECT")),
        "our board and the server's disagree, so every move from here is illegal: {sent:?}"
    );
    assert!(!sent.iter().any(|line| line.starts_with("AGREE")), "{sent:?}");
    assert!(harness.alerts().contains(&AlertCode::UnusableProposal));
}

#[test]
fn a_proposal_whose_to_move_agrees_is_accepted() {
    let mut harness = Harness::new(forever_config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");
    let mark = harness.mark();
    harness.summary(50, RESUMED_PROPOSAL);

    let sent = server_lines(harness.since(mark));
    assert!(sent.iter().any(|line| line.starts_with("AGREE")), "{sent:?}");
    assert!(!sent.iter().any(|line| line.starts_with("REJECT")), "{sent:?}");
}

#[test]
fn malformed_summary_is_discarded_then_rejected_and_the_next_proposal_recovers() {
    let mut harness = waiting_harness();
    let malformed = PROPOSAL.replace("Time_Unit:1sec", "Time_Unit:bogus");
    let mark = harness.mark();
    harness.summary(50, &malformed);

    let rejected = harness.since(mark);
    assert_eq!(server_lines(rejected), ["REJECT"]);
    assert!(!engine_lines(rejected).iter().any(|line| line == "usinewgame"));
    assert!(harness.alerts().contains(&AlertCode::UnreadableServerLine));
    assert!(harness.alerts().contains(&AlertCode::UnusableProposal));

    let recovered = harness.mark();
    harness.summary(60, PROPOSAL);
    assert!(server_lines(harness.since(recovered)).iter().any(|line| line.starts_with("AGREE")));
}

#[test]
fn unterminated_position_is_rejected_and_the_next_proposal_recovers() {
    let mut harness = waiting_harness();
    let malformed = PROPOSAL.replace("END Position\n", "");
    let mark = harness.mark();
    harness.summary(50, &malformed);

    let rejected = harness.since(mark);
    assert_eq!(server_lines(rejected), ["REJECT"]);
    assert!(!engine_lines(rejected).iter().any(|line| line == "usinewgame"));
    assert!(harness.alerts().contains(&AlertCode::UnreadableServerLine));

    let recovered = harness.mark();
    harness.summary(60, PROPOSAL);
    assert!(server_lines(harness.since(recovered)).iter().any(|line| line.starts_with("AGREE")));
}

#[test]
fn missing_empty_and_unknown_positions_are_rejected() {
    for proposal in [
        PROPOSAL.replace("BEGIN Position\nPI\n+\nEND Position\n", ""),
        PROPOSAL.replace("PI\n+", "+"),
        PROPOSAL.replace("PI\n+", "unknown\n+"),
    ] {
        let mut harness = waiting_harness();
        let mark = harness.mark();
        harness.summary(50, &proposal);
        let actions = harness.since(mark);
        assert!(server_lines(actions).iter().any(|line| line.starts_with("REJECT")));
        assert!(!server_lines(actions).iter().any(|line| line.starts_with("AGREE")));
        assert!(!engine_lines(actions).iter().any(|line| line == "usinewgame"));
    }
}

#[test]
fn missing_required_proposal_fields_are_rejected() {
    for required in [
        "Protocol_Version:1.2\n",
        "Format:Shogi 1.0\n",
        "Name+:us\n",
        "Name-:them\n",
        "Your_Turn:+\n",
        "To_Move:+\n",
    ] {
        let mut harness = waiting_harness();
        let proposal = PROPOSAL.replace(required, "");
        let mark = harness.mark();
        harness.summary(50, &proposal);
        let sent = server_lines(harness.since(mark));
        assert!(sent.iter().any(|line| line.starts_with("REJECT")), "{required}: {sent:?}");
        assert!(!sent.iter().any(|line| line.starts_with("AGREE")), "{required}: {sent:?}");
    }
}

#[test]
fn unsupported_protocol_shape_is_rejected() {
    for proposal in [
        PROPOSAL.replace("Protocol_Version:1.2", "Protocol_Version:1.3"),
        PROPOSAL.replace("Protocol_Mode:Server", "Protocol_Mode:Direct"),
        PROPOSAL.replace("Format:Shogi 1.0", "Format:Chess 1.0"),
        PROPOSAL.replace("Rematch_On_Draw:NO", "Rematch_On_Draw:YES"),
    ] {
        let mut harness = waiting_harness();
        let mark = harness.mark();
        harness.summary(50, &proposal);
        let sent = server_lines(harness.since(mark));
        assert!(sent.iter().any(|line| line.starts_with("REJECT")), "{sent:?}");
        assert!(!sent.iter().any(|line| line.starts_with("AGREE")), "{sent:?}");
    }
}

#[test]
fn time_unlimited_proposals_are_rejected_without_starting_the_engine() {
    let time = "BEGIN Time\nTime_Unit:1sec\nTotal_Time:900\nByoyomi:0\nIncrement:0\nLeast_Time_Per_Move:0\nEND Time\n";
    for proposal in [PROPOSAL.replace(time, ""), PROPOSAL.replace(time, "BEGIN Time\nEND Time\n")] {
        let mut harness = waiting_harness();
        let mark = harness.mark();
        harness.summary(50, &proposal);
        let actions = harness.since(mark);
        assert!(server_lines(actions).iter().any(|line| line.starts_with("REJECT")));
        assert!(!engine_lines(actions).iter().any(|line| line == "usinewgame"));
        assert!(go_line(actions).is_none());
    }
}

#[test]
fn leaving_the_pairing_queue_logs_out_rather_than_dropping_the_socket() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    // Waiting to be paired. The connection is still ours to leave.
    let closing = harness.feed(Event::Operator { at: 100, command: OperatorCommand::Quit });

    let sent = server_lines(&closing);
    assert_eq!(sent, ["LOGOUT"], "{sent:?}");
    let logout = closing
        .iter()
        .position(|action| {
            matches!(action, Action::SendServer(command) if format_command(command) == "LOGOUT")
        })
        .expect("logout is sent");
    let shutdown = closing
        .iter()
        .position(|action| matches!(action, Action::Shutdown { .. }))
        .expect("the session closes");
    assert!(logout < shutdown, "the line has to be written before the shell tears down");
}

#[test]
fn a_finished_plain_csa_game_leaves_the_close_to_the_server() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);
    harness.server(100, "#TIME_UP");
    let closing = harness.server(100, "#LOSE");

    // `game.rb:234` calls `player.finish` for a plain-CSA player when the game
    // ends, so the socket is already being torn down.
    assert!(server_lines(&closing).is_empty());
    assert!(closing.iter().any(|action| matches!(action, Action::Shutdown { .. })));
}

#[test]
fn an_x1_session_still_logs_out_after_a_game() {
    let mut config = config();
    config.x1 = true;
    let mut harness = Harness::new(config);
    harness.start_game(PROPOSAL);
    harness.server(100, "#TIME_UP");
    let closing = harness.server(100, "#LOSE");

    // `x1` exists so the connection outlives the game, so leaving it is ours.
    assert_eq!(server_lines(&closing), ["LOGOUT"]);
}

#[test]
fn a_lost_connection_does_not_try_to_log_out() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    let lost = harness.feed(Event::ServerEof { at: 100 });
    assert!(server_lines(&lost).is_empty(), "there is nothing left to write to");
    assert!(harness.alerts().contains(&AlertCode::ConnectionLost));
}

#[test]
fn an_engine_that_offers_no_prediction_is_not_pondered_for() {
    let mut harness = Harness::new(config());
    harness.start_game(PROPOSAL);

    harness.engine(100, "bestmove 7g7f");
    let echo = harness.server(200, "+7776FU,T3");

    assert!(ponder_go_line(&echo).is_none());
    assert!(engine_lines(&echo).is_empty(), "nothing at all happens on the opponent's clock");
}

#[test]
fn usi_ponder_false_is_honoured_even_when_the_engine_ignores_it() {
    let mut config = config();
    config.engine_options = vec![("USI_Ponder".to_owned(), "false".to_owned())];
    let mut harness = Harness::new(config);
    harness.start_game(PROPOSAL);

    // `YaneuraOu` v9.40 offers a ponder move after being told not to ponder, so
    // the option cannot be left for the engine to enforce.
    harness.engine(100, "bestmove 7g7f ponder 3c3d");
    let echo = harness.server(200, "+7776FU,T3");

    assert!(ponder_go_line(&echo).is_none(), "the operator said no");
    assert!(engine_lines(&echo).is_empty());
}

/// A bridge killed mid-game writes no terminal record and no alert: its log
/// simply stops, which a reader cannot tell from a long think. These fix the
/// records that make the difference observable.
#[test]
fn the_engine_says_what_it_is_and_the_log_keeps_it() {
    // A game is not reproducible from the log without the options the engine was
    // actually given: `Threads` and `USI_Hash` change what it plays, and until
    // this record they lived only in the operator's shell history.
    let mut config = config();
    config.engine_options =
        vec![("Threads".to_owned(), "4".to_owned()), ("USI_Hash".to_owned(), "1024".to_owned())];
    let mut harness = Harness::new(config);
    harness.feed(Event::Started { at: 0 });
    harness.engine(5, "id name YaneuraOu NNUE 8.30");
    harness.engine(6, "id author Motohiro Isozaki");
    harness.engine(10, "usiok");

    let records = harness.records();
    let meta = records
        .iter()
        .find_map(|record| match record {
            RecordBody::EngineMeta { name, author, options } => Some((name, author, options)),
            _ => None,
        })
        .expect("usiok should record what the engine turned out to be");

    assert_eq!(meta.0.as_deref(), Some("YaneuraOu NNUE 8.30"));
    assert_eq!(meta.1.as_deref(), Some("Motohiro Isozaki"));
    assert_eq!(
        meta.2
            .iter()
            .map(|option| (option.name.as_str(), option.value.as_str()))
            .collect::<Vec<_>>(),
        [("Threads", "4"), ("USI_Hash", "1024")],
        "the options are recorded as sent, in the order they were sent",
    );
}

#[test]
fn a_restarted_engine_does_not_re_announce_an_unchanged_identity() {
    // A restart already has its own alert. Repeating the identity would say
    // something happened to the engine when nothing about it changed.
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(5, "id name Test Engine");
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    harness.feed(Event::EngineExited { at: 50 });
    harness.engine(60, "id name Test Engine");
    harness.engine(70, "usiok");

    let announcements = harness
        .records()
        .iter()
        .filter(|record| matches!(record, RecordBody::EngineMeta { .. }))
        .count();
    assert_eq!(announcements, 1, "the engine identity is stated once per run");
}

#[test]
fn a_quiet_bridge_writes_a_heartbeat_so_silence_means_something() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    let before = harness.records().len();

    // Nothing has been logged for less than the interval: still quiet.
    harness.tick(40 + 59_000);
    assert_eq!(harness.records().len(), before, "no heartbeat before the interval elapses");

    harness.tick(40 + 60_000);
    assert!(
        matches!(harness.records().last(), Some(RecordBody::Heartbeat)),
        "a heartbeat fills the silence once the interval has passed",
    );
}

#[test]
fn heartbeats_fill_silence_rather_than_beat_on_a_fixed_pulse() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");

    let first = 40 + 60_000;
    harness.tick(first);
    let after_first = harness.records().len();

    // The heartbeat is itself a record, so it restarts the interval. This is
    // what keeps a game in progress — where moves already prove liveness — from
    // paying for heartbeats it does not need.
    harness.tick(first + 59_000);
    assert_eq!(harness.records().len(), after_first, "the clock runs from the last record written");

    harness.tick(first + 60_000);
    assert_eq!(harness.records().len(), after_first + 1, "and fires again a full interval later");
}

#[test]
fn a_deliberate_shutdown_says_so_after_closing() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");
    harness.feed(Event::Operator { at: 50, command: OperatorCommand::Quit });

    let records = harness.records();
    let tail: Vec<_> = records.iter().rev().take(2).collect();
    assert!(
        matches!(tail.as_slice(), [RecordBody::BridgeStop, RecordBody::State { phase: Phase::Closing }]),
        "closing marks the start of shutdown; bridge_stop marks its completion, and is last: {records:?}",
    );
}

#[test]
fn a_closed_bridge_stops_writing_heartbeats() {
    let mut harness = Harness::new(config());
    harness.feed(Event::Started { at: 0 });
    harness.engine(10, "usiok");
    harness.engine(20, "readyok");
    harness.feed(Event::Connected { at: 30 });
    harness.server(40, "LOGIN:us OK");
    harness.feed(Event::Operator { at: 50, command: OperatorCommand::Quit });

    let after_stop = harness.records().len();
    harness.tick(50 + 120_000);
    assert_eq!(
        harness.records().len(),
        after_stop,
        "a log that ended with bridge_stop must not grow afterwards",
    );
}
