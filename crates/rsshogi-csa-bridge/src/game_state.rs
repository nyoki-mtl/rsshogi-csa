//! Board and clock, kept in one place so they cannot drift.
//!
//! If the position and the clock ledger disagree, the game is lost quietly: a
//! stale board makes the legality gate approve an illegal move, and a stale
//! ledger runs the clock out. So neither is exposed for mutation. The only way
//! to advance either is [`GameState::apply_csa_move`], which advances both.
//!
//! [`GameState::csa_for_usi`] is the other half of the same idea: it is the only
//! way to turn an engine's answer into text for the server, and it refuses to
//! produce that text for a move the position does not accept. A move we send
//! cannot be illegal, because there is no code path that produces one.

use std::error::Error;
use std::fmt;
use std::sync::Once;

use rsshogi::board::{
    move_from_usi, movegen::generate_legal_all_move32, position_from_sfen, Move32List, Position,
};
use rsshogi::records::formats::csa as csa_record;
use rsshogi::types::{Color as ShogiColor, EnteringKingRule, Move32, RepetitionState, MOVE_NONE};
use rsshogi_csa::{Color, CsaMove, GameSummary};

use crate::clock::{ClockError, ClockLedger, UsiGoTimes};

static BOARD_INIT: Once = Once::new();

fn ensure_board_tables() {
    BOARD_INIT.call_once(rsshogi::board::init);
}

const fn from_shogi_color(color: ShogiColor) -> Color {
    match color {
        ShogiColor::Black => Color::Black,
        ShogiColor::White => Color::White,
    }
}

/// A move that has been played, with everything the event log needs about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayedMove {
    /// 1-based move number.
    pub ply: u32,
    /// Who played it.
    pub color: Color,
    /// The move as it appeared on the wire, without annotations.
    pub csa: String,
    /// The same move in USI spelling.
    pub usi: String,
    /// What the server charged, in `Time_Unit` units.
    pub charged_units: u64,
    /// The same charge in milliseconds.
    pub charged_ms: u64,
    /// Black's remaining main time after the move, in milliseconds.
    pub black_remaining_ms: u64,
    /// White's remaining main time after the move, in milliseconds.
    pub white_remaining_ms: u64,
}

/// Something the board or the clock refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameStateError {
    /// The time declaration could not be turned into a ledger.
    Clock(ClockError),
    /// The proposal did not say which side we play.
    MissingYourTurn,
    /// The `BEGIN Position` body could not be read as a position.
    Position(String),
    /// A move string is not a move in this position.
    UnparsableMove(String),
    /// A move is syntactically fine but not legal here.
    IllegalMove(String),
    /// A legal move has no CSA spelling, which should not happen.
    NoCsaSpelling(String),
}

impl fmt::Display for GameStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clock(err) => write!(f, "time control is unusable: {err}"),
            Self::MissingYourTurn => f.write_str("game summary has no `Your_Turn`"),
            Self::Position(detail) => write!(f, "position block is unreadable: {detail}"),
            Self::UnparsableMove(mv) => write!(f, "move is not readable here: `{mv}`"),
            Self::IllegalMove(mv) => write!(f, "move is not legal here: `{mv}`"),
            Self::NoCsaSpelling(mv) => write!(f, "move has no csa spelling: `{mv}`"),
        }
    }
}

impl Error for GameStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Clock(err) => Some(err),
            _ => None,
        }
    }
}

impl From<ClockError> for GameStateError {
    fn from(value: ClockError) -> Self {
        Self::Clock(value)
    }
}

/// The entering-king rule a proposal implies.
///
/// `Declaration:Jishogi 1.1` is the only declaration line any targeted server
/// sends, and it means the CSA 27-point rule. Anything else is treated as "no
/// declaration", which only ever costs us a win we could have claimed — the
/// opposite mistake forfeits the game on an illegal action.
#[must_use]
pub fn entering_king_rule_for(summary: &GameSummary) -> EnteringKingRule {
    match summary.declaration.as_deref() {
        Some(value) if value.starts_with("Jishogi") => EnteringKingRule::Point27,
        _ => EnteringKingRule::None,
    }
}

/// The board, the clock, and the move history, advanced together.
#[derive(Clone)]
pub struct GameState {
    position: Position,
    ledger: ClockLedger,
    our_color: Color,
    initial_sfen: String,
    history: Vec<PlayedMove>,
}

impl fmt::Debug for GameState {
    /// `Position` has no `Debug`, so the board is summarized by the moves that
    /// produced it, which is what a failing test wants to read anyway.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GameState")
            .field("our_color", &self.our_color)
            .field("initial_sfen", &self.initial_sfen)
            .field("moves", &self.history.iter().map(|played| &played.usi).collect::<Vec<_>>())
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl GameState {
    /// Builds the state a proposal describes, before any move is replayed.
    ///
    /// Moves inside `BEGIN Position` are deliberately *not* applied here. Feed
    /// them to [`GameState::apply_csa_move`] so replay and live play take the
    /// same path.
    pub fn from_summary(
        summary: &GameSummary,
        rule: EnteringKingRule,
    ) -> Result<Self, GameStateError> {
        ensure_board_tables();

        let our_color = summary.your_turn.ok_or(GameStateError::MissingYourTurn)?;
        let ledger = ClockLedger::from_summary(summary)?;

        let (board_lines, _) = summary.position_split();
        let record_text = format!("V2.2\n{}\n", board_lines.join("\n"));
        let record = csa_record::parse_csa_str(&record_text)
            .map_err(|err| GameStateError::Position(err.to_string()))?;
        let initial_sfen = record.initial_position().to_sfen().to_owned();

        let mut position = position_from_sfen(&initial_sfen)
            .map_err(|err| GameStateError::Position(err.to_string()))?;
        position.set_entering_king_rule(rule);

        Ok(Self { position, ledger, our_color, initial_sfen, history: Vec::new() })
    }

    /// The side we play.
    #[must_use]
    pub const fn our_color(&self) -> Color {
        self.our_color
    }

    /// The side to move.
    #[must_use]
    pub const fn side_to_move(&self) -> Color {
        from_shogi_color(self.position.turn())
    }

    /// Whether it is our turn.
    #[must_use]
    pub const fn is_our_turn(&self) -> bool {
        matches!(
            (self.side_to_move(), self.our_color),
            (Color::Black, Color::Black) | (Color::White, Color::White)
        )
    }

    /// How many moves have been played.
    #[must_use]
    pub fn ply(&self) -> u32 {
        u32::try_from(self.history.len()).unwrap_or(u32::MAX)
    }

    /// The position before any move was played.
    #[must_use]
    pub fn initial_sfen(&self) -> &str {
        &self.initial_sfen
    }

    /// Every move played so far.
    #[must_use]
    pub fn history(&self) -> &[PlayedMove] {
        &self.history
    }

    /// Read-only access to the board.
    #[must_use]
    pub const fn position(&self) -> &Position {
        &self.position
    }

    /// Read-only access to the clock.
    #[must_use]
    pub const fn ledger(&self) -> &ClockLedger {
        &self.ledger
    }

    /// USI `go` times for our own search.
    #[must_use]
    pub const fn usi_times(&self, margin_ms: u64) -> UsiGoTimes {
        self.ledger.usi_times(self.our_color, margin_ms)
    }

    /// The `position` command that puts an engine on the current board.
    #[must_use]
    pub fn usi_position_command(&self) -> String {
        let mut out = format!("position sfen {}", self.initial_sfen);
        if !self.history.is_empty() {
            out.push_str(" moves");
            for played in &self.history {
                out.push(' ');
                out.push_str(&played.usi);
            }
        }
        out
    }

    /// The `position` command for the board one hypothetical move further on.
    ///
    /// A ponder searches a position the opponent has not reached yet, so the
    /// move must not touch the state. Pass it through [`GameState::csa_for_usi`]
    /// first: this only spells the command out, it does not judge the move.
    #[must_use]
    pub fn usi_position_command_with(&self, extra_usi: &str) -> String {
        let mut out = self.usi_position_command();
        if self.history.is_empty() {
            out.push_str(" moves");
        }
        out.push(' ');
        out.push_str(extra_usi);
        out
    }

    /// Whether the side to move may declare an entering-king win right now.
    ///
    /// This is evaluated under the rule the proposal implied, not under whatever
    /// the engine happens to be configured with.
    #[must_use]
    pub fn can_declare_win(&self) -> bool {
        self.position.evaluate_declaration().can_declare
    }

    /// The repetition state of the current position.
    ///
    /// CSA servers adjudicate repetition themselves, so this is for logging and
    /// for a resign policy, never for terminating the game ourselves.
    #[must_use]
    pub fn repetition_state(&self) -> RepetitionState {
        self.position.repetition_state()
    }

    /// Turns an engine's USI answer into the text to send, or refuses.
    ///
    /// This is the legality gate. Every move that reaches the socket comes
    /// through here, so an illegal move of our own is unrepresentable rather
    /// than merely unlikely.
    ///
    /// `Move32::to_csa` spells the move with its side sign, which it takes from
    /// the moved piece's colour rather than from the board. Re-validating the
    /// result as a [`CsaMove`] is still part of the gate, so the codec can never
    /// be asked to send something it would reject.
    pub fn csa_for_usi(&self, usi: &str) -> Result<String, GameStateError> {
        let mv = move_from_usi(&self.position, usi)
            .ok_or_else(|| GameStateError::UnparsableMove(usi.to_owned()))?;
        if !self.position.is_legal_move32(mv) {
            return Err(GameStateError::IllegalMove(usi.to_owned()));
        }
        let signed = mv.to_csa().ok_or_else(|| GameStateError::NoCsaSpelling(usi.to_owned()))?;
        CsaMove::parse(&signed).ok_or_else(|| GameStateError::NoCsaSpelling(usi.to_owned()))?;
        Ok(signed)
    }

    /// The principal variation spelled for a move comment, in CSA.
    ///
    /// Floodgate records `,'* <score> <pv...>` and shows it on the game page.
    /// The variation is read from the board *after* `csa`, because what is
    /// recorded is the continuation and not a repeat of the move it hangs off.
    /// An engine's variation normally opens with that same move, so a leading
    /// repeat is dropped.
    ///
    /// Conversion stops at the first move the board will not take. A hash
    /// collision can leave rubbish at the tail of a variation, and a truncated
    /// line is worth keeping where a wrong one is not.
    #[must_use]
    pub fn csa_pv_after(&self, csa: &str, pv_usi: &[String], limit: usize) -> Vec<String> {
        let mut position = self.position.clone();
        let played = position.move_from_csa(csa);
        if played == MOVE_NONE || !position.is_legal_move32(played) {
            return Vec::new();
        }
        position.apply_move32(played);

        let mut rest = pv_usi;
        if let Some(first) = pv_usi.first() {
            if move_from_usi(&self.position, first) == Some(played) {
                rest = &pv_usi[1..];
            }
        }

        let mut out = Vec::new();
        for usi in rest.iter().take(limit) {
            let Some(mv) = move_from_usi(&position, usi) else {
                break;
            };
            if !position.is_legal_move32(mv) {
                break;
            }
            let Some(signed) = mv.to_csa() else {
                break;
            };
            if CsaMove::parse(&signed).is_none() {
                break;
            }
            out.push(signed);
            position.apply_move32(mv);
        }
        out
    }

    /// Applies one move and charges the clock, in one step.
    ///
    /// The move must be legal; a rejected move leaves the state untouched, so a
    /// caller that logs the error can carry on with a consistent board.
    pub fn apply_csa_move(
        &mut self,
        csa: &str,
        charged_units: u64,
    ) -> Result<PlayedMove, GameStateError> {
        let mv = self.position.move_from_csa(csa);
        if mv == MOVE_NONE {
            return Err(GameStateError::UnparsableMove(csa.to_owned()));
        }
        if !self.position.is_legal_move32(mv) {
            return Err(GameStateError::IllegalMove(csa.to_owned()));
        }

        let color = self.side_to_move();
        let usi = Self::usi_of(mv);
        let charged_ms = charged_units.saturating_mul(self.ledger.rules(color).unit_ms());

        self.position.apply_move32(mv);
        self.ledger.apply_move(color, charged_units);

        let played = PlayedMove {
            ply: self.ply().saturating_add(1),
            color,
            csa: csa.to_owned(),
            usi,
            charged_units,
            charged_ms,
            black_remaining_ms: self.ledger.remaining_ms(Color::Black),
            white_remaining_ms: self.ledger.remaining_ms(Color::White),
        };
        self.history.push(played.clone());
        Ok(played)
    }

    /// Re-applies the clock figures the proposal stated, after replaying its moves.
    ///
    /// `Remaining_Time+` / `Remaining_Time-` are the clock as it stands, not as
    /// it stood before the moves in the position block. Replaying those moves on
    /// top of the declared figures would charge the same time twice, so the
    /// declaration is applied last and wins.
    pub const fn sync_declared_remaining(&mut self, summary: &GameSummary) {
        if let Some(units) = summary.time_black.remaining_time {
            self.ledger.set_remaining_units(Color::Black, units);
        }
        if let Some(units) = summary.time_white.remaining_time {
            self.ledger.set_remaining_units(Color::White, units);
        }
    }

    /// Whether the side to move has any legal move, independently of engine scores.
    ///
    /// Having no legal move justifies resignation even without a score. This
    /// does not by itself distinguish checkmate from a position without check.
    #[must_use]
    pub fn has_legal_moves(&self) -> bool {
        let mut list = Move32List::new();
        generate_legal_all_move32(&self.position, &mut list);
        !list.is_empty()
    }

    /// Any legal move, spelled for the wire.
    ///
    /// Only the panic branch of the fallback ladder uses this, and only when the
    /// operator opted into it. Returns `None` when there is no legal move, which
    /// means the game is over anyway.
    #[must_use]
    pub fn first_legal_csa(&self) -> Option<String> {
        let mut list = Move32List::new();
        generate_legal_all_move32(&self.position, &mut list);
        let mv = list.iter().next().copied()?;
        let signed = mv.to_csa()?;
        CsaMove::parse(&signed)?;
        Some(signed)
    }

    fn usi_of(mv: Move32) -> String {
        mv.to_usi()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsshogi_csa::GameSummaryReader;

    const HIRATE: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:test+floodgate-900-0+black_engine+white_engine+20260804120000
Name+:black_engine
Name-:white_engine
Your_Turn:+
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

    const BUOY: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Declaration:Jishogi 1.1
Game_ID:test+buoy_opening-1500-0+black_engine+white_engine+20260804123000
Name+:black_engine
Name-:white_engine
Your_Turn:-
To_Move:+
BEGIN Time
Time_Unit:1sec
Total_Time:1500
Byoyomi:0
Increment:0
Least_Time_Per_Move:0
END Time
BEGIN Position
PI
+
+7776FU,T2
-3334FU,T5
END Position
END Game_Summary";

    fn read(block: &str) -> GameSummary {
        let mut reader = GameSummaryReader::new();
        let mut summary = None;
        for line in block.lines() {
            if let Some(done) = reader.feed(line).expect("fixture should be accepted") {
                summary = Some(done);
            }
        }
        summary.expect("fixture should be complete")
    }

    fn state(block: &str) -> GameState {
        let summary = read(block);
        let rule = entering_king_rule_for(&summary);
        GameState::from_summary(&summary, rule).expect("state should build")
    }

    #[test]
    fn hirate_proposal_builds_the_start_position() {
        let game = state(HIRATE);
        assert_eq!(game.our_color(), Color::Black);
        assert_eq!(game.side_to_move(), Color::Black);
        assert!(game.is_our_turn());
        assert_eq!(game.ply(), 0);
        assert!(game.history().is_empty());
        assert_eq!(game.usi_position_command(), format!("position sfen {}", game.initial_sfen()));
    }

    #[test]
    fn applying_a_move_advances_board_and_clock_together() {
        let mut game = state(HIRATE);
        let played = game.apply_csa_move("+7776FU", 12).expect("legal opening move");

        assert_eq!(played.ply, 1);
        assert_eq!(played.color, Color::Black);
        assert_eq!(played.usi, "7g7f");
        assert_eq!(played.charged_units, 12);
        assert_eq!(played.charged_ms, 12_000);
        assert_eq!(played.black_remaining_ms, 888_000);
        assert_eq!(played.white_remaining_ms, 900_000);

        assert_eq!(game.side_to_move(), Color::White);
        assert!(!game.is_our_turn());
        assert_eq!(game.ply(), 1);
        assert_eq!(game.ledger().remaining_units(Color::Black), 888);
    }

    #[test]
    fn an_illegal_move_leaves_the_state_untouched() {
        let mut game = state(HIRATE);
        let before = game.clone();

        // A pawn cannot jump two squares.
        let err = game.apply_csa_move("+7775FU", 3).expect_err("should be rejected");
        assert!(matches!(err, GameStateError::IllegalMove(_) | GameStateError::UnparsableMove(_)));

        assert_eq!(game.ply(), before.ply());
        assert_eq!(game.side_to_move(), before.side_to_move());
        assert_eq!(
            game.ledger().remaining_units(Color::Black),
            before.ledger().remaining_units(Color::Black)
        );
    }

    #[test]
    fn a_move_by_the_wrong_side_is_rejected() {
        let mut game = state(HIRATE);
        // Black is to move, so a white move must not be accepted.
        let err = game.apply_csa_move("-3334FU", 1).expect_err("should be rejected");
        assert_eq!(err, GameStateError::UnparsableMove("-3334FU".to_owned()));
    }

    #[test]
    fn the_legality_gate_refuses_to_spell_an_illegal_move() {
        let game = state(HIRATE);
        assert_eq!(game.csa_for_usi("7g7f").expect("legal"), "+7776FU");
        assert!(game.csa_for_usi("7g7e").is_err(), "a two square pawn push is not legal");
        assert!(game.csa_for_usi("resign").is_err(), "a sentinel is not a move");
        assert!(game.csa_for_usi("").is_err());
        assert!(game.csa_for_usi("zz9z").is_err());
    }

    #[test]
    fn the_gate_tracks_the_position_rather_than_the_opening() {
        let mut game = state(HIRATE);
        game.apply_csa_move("+7776FU", 1).expect("legal");
        // It is white's turn now, so a black move no longer spells out.
        assert!(game.csa_for_usi("2g2f").is_err());
        assert_eq!(game.csa_for_usi("3c3d").expect("legal"), "-3334FU");
    }

    #[test]
    fn replaying_a_buoy_position_lands_on_the_live_numbers() {
        let summary = read(BUOY);
        let rule = entering_king_rule_for(&summary);
        let mut game = GameState::from_summary(&summary, rule).expect("state should build");
        assert_eq!(game.our_color(), Color::White);

        let (_, moves) = summary.position_split();
        for line in moves {
            let (csa, charged) = line.split_once(",T").expect("replayed move carries its time");
            let charged: u64 = charged.parse().expect("time is numeric");
            game.apply_csa_move(csa, charged).expect("replayed move should be legal");
        }

        assert_eq!(game.ply(), 2);
        assert_eq!(game.side_to_move(), Color::Black);
        assert_eq!(game.ledger().remaining_units(Color::Black), 1_498);
        assert_eq!(game.ledger().remaining_units(Color::White), 1_495);
        assert!(game.usi_position_command().ends_with(" moves 7g7f 3c3d"));
    }

    #[test]
    fn the_pv_comment_carries_the_continuation_not_the_move_itself() {
        let game = state(HIRATE);
        let pv = ["7g7f", "3c3d", "2g2f", "8c8d"].map(str::to_owned);

        // The engine's variation opens with the move being sent, and floodgate
        // records what comes after it.
        let moves = game.csa_pv_after("+7776FU", &pv, 32);
        assert_eq!(moves, ["-3334FU", "+2726FU", "-8384FU"]);
    }

    #[test]
    fn a_pv_that_does_not_repeat_the_move_is_taken_whole() {
        let game = state(HIRATE);
        let pv = ["3c3d", "2g2f"].map(str::to_owned);
        assert_eq!(game.csa_pv_after("+7776FU", &pv, 32), ["-3334FU", "+2726FU"]);
    }

    #[test]
    fn the_pv_comment_stops_at_the_first_move_the_board_refuses() {
        let game = state(HIRATE);
        // A hash collision can leave rubbish at the tail of a variation.
        let pv = ["7g7f", "3c3d", "9i9a", "2g2f"].map(str::to_owned);
        assert_eq!(game.csa_pv_after("+7776FU", &pv, 32), ["-3334FU"]);
    }

    #[test]
    fn the_pv_comment_respects_its_limit_and_its_inputs() {
        let game = state(HIRATE);
        let pv = ["7g7f", "3c3d", "2g2f", "8c8d"].map(str::to_owned);
        assert_eq!(game.csa_pv_after("+7776FU", &pv, 1), ["-3334FU"]);
        assert!(game.csa_pv_after("+7776FU", &pv, 0).is_empty());
        assert!(game.csa_pv_after("+7776FU", &[], 32).is_empty());
        // A move the board will not take yields nothing rather than a wrong line.
        assert!(game.csa_pv_after("+7775FU", &pv, 32).is_empty());
    }

    #[test]
    fn declaration_uses_the_rule_the_summary_implied() {
        let summary = read(HIRATE);
        assert_eq!(entering_king_rule_for(&summary), EnteringKingRule::Point27);

        let mut without = read(HIRATE);
        without.declaration = None;
        assert_eq!(entering_king_rule_for(&without), EnteringKingRule::None);

        // Nobody can declare from the starting position under either rule.
        let game = state(HIRATE);
        assert!(!game.can_declare_win());
    }

    #[test]
    fn a_summary_without_your_turn_is_refused() {
        let mut summary = read(HIRATE);
        summary.your_turn = None;
        let err = GameState::from_summary(&summary, EnteringKingRule::Point27)
            .expect_err("we must know which side we are");
        assert_eq!(err, GameStateError::MissingYourTurn);
    }
}
