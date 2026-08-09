//! Rebuilding the clock from a proposal, including one that carries moves.
//!
//! A buoy game, and a game an operator re-opens after a restart, arrive as a
//! `Game_Summary` whose `BEGIN Position` block already contains the moves played
//! with their `,T` values. Replay has to land on the same numbers a live game
//! would have reached, which is why replay and live play share `apply_move`.

use rsshogi_csa::{parse_line, Color, CsaCommand, GameSummary, GameSummaryReader};
use rsshogi_csa_bridge::{ClockLedger, MoveBudget};

const BUOY_PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+buoy_opening-1500-60+black_engine+white_engine+20260804123000
Name+:black_engine
Name-:white_engine
Your_Turn:-
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:1500
Byoyomi:60
Increment:0
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
+7776FU,T2
-3334FU,T5
+2726FU,T1
END Position
END Game_Summary";

const FISCHER_PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Game_ID:wdoor+floodgate-600-10F+black_engine+white_engine+20260804140000
Name+:black_engine
Name-:white_engine
Your_Turn:+
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:600
Byoyomi:0
Increment:10
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
+7776FU,T30
-3334FU,T3
END Position
END Game_Summary";

const RESUMED_WITH_REMAINING: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Game_ID:test+resumed-1500-0+black_engine+white_engine+20260804150000
Name+:black_engine
Name-:white_engine
Your_Turn:+
To_Move:+
BEGIN Time
Time_Unit:1sec
Total_Time:1500
Byoyomi:0
Increment:0
Least_Time_Per_Move:0
Remaining_Time+:874
Remaining_Time-:889
END Time
END Game_Summary";

#[test]
fn replaying_a_buoy_position_reproduces_both_clocks() {
    let summary = read(BUOY_PROPOSAL);
    let mut ledger = ClockLedger::from_summary(&summary).expect("time control should be usable");

    assert_eq!(ledger.remaining_units(Color::Black), 1_500);
    assert_eq!(ledger.remaining_units(Color::White), 1_500);

    replay(&summary, &mut ledger);

    // Black spent 2 + 1, White spent 5.
    assert_eq!(ledger.remaining_units(Color::Black), 1_497);
    assert_eq!(ledger.remaining_units(Color::White), 1_495);

    // Byoyomi is a per-move allowance, so it shows up in the budget only.
    assert_eq!(ledger.budget(Color::White), MoveBudget::Units(1_554));
    assert_eq!(ledger.budget_ms(Color::White), Some(1_554_999));
}

#[test]
fn replay_credits_the_increment_exactly_as_live_play_would() {
    let summary = read(FISCHER_PROPOSAL);
    let mut ledger = ClockLedger::from_summary(&summary).expect("time control should be usable");
    replay(&summary, &mut ledger);

    // 600 + 10 - 30 for black, 600 + 10 - 3 for white.
    assert_eq!(ledger.remaining_units(Color::Black), 580);
    assert_eq!(ledger.remaining_units(Color::White), 607);

    let times = ledger.usi_times(Color::Black, 2_500);
    assert_eq!(times.btime_ms, 577_500);
    assert_eq!(times.wtime_ms, 607_000);
    assert_eq!(times.binc_ms, Some(10_000));
    assert_eq!(times.byoyomi_ms, None);
}

#[test]
fn remaining_time_fields_win_over_total_time() {
    let summary = read(RESUMED_WITH_REMAINING);
    let ledger = ClockLedger::from_summary(&summary).expect("time control should be usable");

    assert_eq!(ledger.remaining_units(Color::Black), 874);
    assert_eq!(ledger.remaining_units(Color::White), 889);
    assert_eq!(ledger.rules(Color::Black).total_time_units, 1_500);

    let times = ledger.usi_times(Color::Black, 0);
    assert_eq!(times.btime_ms, 874_000);
    assert_eq!(times.wtime_ms, 889_000);
    assert_eq!(times.byoyomi_ms, None, "sudden death declares neither form");
}

#[test]
fn a_proposal_without_moves_leaves_the_clocks_at_their_totals() {
    let summary = read(RESUMED_WITH_REMAINING);
    let (board, moves) = summary.position_split();
    assert!(board.is_empty());
    assert!(moves.is_empty());
}

fn replay(summary: &GameSummary, ledger: &mut ClockLedger) {
    let (_, moves) = summary.position_split();
    for line in moves {
        let CsaCommand::Move(parsed) = parse_line(line).expect("replayed move should parse") else {
            panic!("expected a move line, got `{line}`");
        };
        let charged = parsed.elapsed.expect("a replayed move carries its consumed time");
        ledger.apply_move(parsed.csa.color(), charged);
    }
}

fn read(block: &str) -> GameSummary {
    let mut reader = GameSummaryReader::new();
    let mut summary = None;
    for line in block.lines() {
        if let Some(done) = reader.feed(line).expect("fixture block should be accepted") {
            summary = Some(done);
        }
    }
    summary.expect("fixture block should be complete")
}
