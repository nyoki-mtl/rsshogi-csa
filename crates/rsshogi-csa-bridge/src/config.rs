//! Bridge policy.
//!
//! Everything here is a knob that changes how the session behaves without
//! changing what the protocol allows. Defaults are chosen for floodgate over
//! the open internet; a LAN tournament wants a smaller margin.

use rsshogi::types::EnteringKingRule;
use rsshogi_csa::TurnPreference;

/// How long the event log may stay silent before the bridge writes a heartbeat.
///
/// This is the number a reader's liveness check is built on: a live bridge never
/// leaves a gap longer than this, so a longer gap means it is gone. Readers
/// should allow at least two intervals before calling a run dead, since their own
/// polling and the writer's flush both add delay.
///
/// Not a knob. A reader cannot negotiate the interval with the writer, so making
/// it configurable would only let one side quietly invalidate the other's
/// threshold.
pub const HEARTBEAT_INTERVAL_MS: u64 = 60_000;

/// What to do when the fallback ladder runs out of ideas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanicPolicy {
    /// Resign. A move the engine never chose looks broken and rarely saves anything.
    #[default]
    Resign,
    /// Play the first legal move. For people who would rather lose slowly.
    AnyLegal,
}

/// How the bridge logs in and how it behaves when things go wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    /// `LOGIN` name.
    pub login_name: String,
    /// `LOGIN` password. In CSA mode this field also carries the game name,
    /// for example `floodgate-900-0,secret`.
    pub password: String,
    /// Request the `x1` extension, which keeps the connection open between games.
    pub x1: bool,
    /// Game name for the explicit `%%GAME` that `x1` mode requires.
    pub game_name: Option<String>,
    /// Requested turn for `%%GAME`.
    pub turn: TurnPreference,
    /// `setoption` pairs sent once, before `isready`.
    ///
    /// Pondering has no switch of its own. `USI_Ponder` in this list is the one,
    /// and [`BridgeConfig::ponder_enabled`] is how the bridge reads it.
    pub engine_options: Vec<(String, String)>,
    /// Subtracted from our own clock when telling the engine how long it has.
    ///
    /// Covers the network round trip, the server's flooring, and the time
    /// between `bestmove` and the socket write. `usiToCsa.rb` uses 2500 ms.
    pub margin_ms: u64,
    /// How long to wait for a `bestmove` after sending `stop`.
    pub stop_grace_ms: u64,
    /// Blank-line keepalive interval. The CSA rule is a 30 second minimum.
    pub keepalive_interval_ms: u64,
    /// The score above which `bestmove resign` is worth an alert.
    ///
    /// Reporting only. When to resign is the engine's decision and it is always
    /// passed on; this decides nothing except whether the log says the score did
    /// not explain it.
    pub resign_threshold_cp: i32,
    /// Resign after this many consecutive moves the engine failed to produce.
    pub max_degraded_moves: u32,
    /// What to do when nothing better is available.
    pub panic_policy: PanicPolicy,
    /// Overrides the entering-king rule implied by the proposal.
    pub entering_king_rule: Option<EnteringKingRule>,
    /// Attach `,'* <score>` to our moves, which floodgate records.
    pub send_score_comment: bool,
    /// How many principal variation moves to attach after the score.
    ///
    /// Floodgate records `,'* <score> <pv...>` and shows the variation on the
    /// game page. `0` sends the score alone. The cap only guards against an
    /// engine that reports an enormous variation; the reference implementations
    /// send the whole thing.
    pub pv_comment_moves: usize,
    /// How many games to play before shutting down. `0` means keep going.
    pub games: u32,
    /// How many times to restart a dead engine within one game.
    ///
    /// Restarting is never worse than resigning. If the replacement is ready in
    /// time we keep playing; if it is not, the hang ladder resigns exactly as it
    /// would have anyway. A GPU engine will usually lose that race, and losing
    /// it costs nothing.
    pub max_engine_restarts: u32,
}

impl BridgeConfig {
    /// A configuration for one floodgate account, with the safe defaults.
    #[must_use]
    pub fn floodgate(login_name: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            login_name: login_name.into(),
            password: password.into(),
            x1: false,
            game_name: None,
            turn: TurnPreference::Either,
            engine_options: Vec::new(),
            margin_ms: 2_500,
            stop_grace_ms: 1_000,
            keepalive_interval_ms: 40_000,
            resign_threshold_cp: 0,
            max_degraded_moves: 2,
            panic_policy: PanicPolicy::Resign,
            entering_king_rule: None,
            send_score_comment: true,
            pv_comment_moves: 32,
            games: 0,
            max_engine_restarts: 2,
        }
    }

    /// Whether to act on a `ponder` move the engine offers.
    ///
    /// This is not a second switch. It reads the `USI_Ponder` value already in
    /// [`BridgeConfig::engine_options`], so the operator states the intent once,
    /// in the place engine settings live.
    ///
    /// The bridge has to enforce it rather than leave it to the engine.
    /// `YaneuraOu` v9.40 returns `bestmove 2g2f ponder 8c8d` after being sent
    /// `setoption name USI_Ponder value false` — measured, not assumed. For that
    /// engine the option changes time management and nothing else, so an engine
    /// that was told not to ponder can still hand us a move to ponder on.
    ///
    /// When the option is absent the engine's own default governs, and we follow
    /// whatever it offers.
    #[must_use]
    pub fn ponder_enabled(&self) -> bool {
        self.engine_options
            .iter()
            .find(|(name, _)| name == "USI_Ponder")
            .is_none_or(|(_, value)| value.eq_ignore_ascii_case("true"))
    }

    /// The keepalive interval, never below the protocol's 30 second floor.
    #[must_use]
    pub const fn effective_keepalive_ms(&self) -> u64 {
        if self.keepalive_interval_ms < 30_000 {
            30_000
        } else {
            self.keepalive_interval_ms
        }
    }
}

/// The `EnteringKingRule` value name a USI engine expects.
///
/// The combo box is shared by `rsshogi-nnue` and `rsshogi-az` and follows
/// `YaneuraOu`, so the names are fixed.
#[must_use]
pub const fn usi_entering_king_rule(rule: EnteringKingRule) -> &'static str {
    match rule {
        EnteringKingRule::None | EnteringKingRule::Unset => "NoEnteringKing",
        EnteringKingRule::Point24 => "CSARule24",
        EnteringKingRule::Point24Handicap => "CSARule24H",
        EnteringKingRule::Point27 => "CSARule27",
        EnteringKingRule::Point27Handicap => "CSARule27H",
        EnteringKingRule::TryRule => "TryRule",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_never_goes_below_the_protocol_floor() {
        let mut config = BridgeConfig::floodgate("engine", "secret");
        assert_eq!(config.effective_keepalive_ms(), 40_000);
        config.keepalive_interval_ms = 5_000;
        assert_eq!(config.effective_keepalive_ms(), 30_000);
    }

    #[test]
    fn ponder_follows_the_engine_option_it_was_given() {
        let mut config = BridgeConfig::floodgate("engine", "secret");
        assert!(config.ponder_enabled(), "with nothing said, the engine's own default governs");

        config.engine_options = vec![("USI_Ponder".to_owned(), "false".to_owned())];
        assert!(!config.ponder_enabled());

        config.engine_options = vec![("USI_Ponder".to_owned(), "true".to_owned())];
        assert!(config.ponder_enabled());

        // Engines are not consistent about case here.
        config.engine_options = vec![("USI_Ponder".to_owned(), "True".to_owned())];
        assert!(config.ponder_enabled());

        // Anything that is not a yes is a no.
        config.engine_options = vec![("USI_Ponder".to_owned(), String::new())];
        assert!(!config.ponder_enabled());
    }

    #[test]
    fn entering_king_rule_names_match_the_engine_combo() {
        assert_eq!(usi_entering_king_rule(EnteringKingRule::Point27), "CSARule27");
        assert_eq!(usi_entering_king_rule(EnteringKingRule::None), "NoEnteringKing");
        assert_eq!(usi_entering_king_rule(EnteringKingRule::Unset), "NoEnteringKing");
        assert_eq!(usi_entering_king_rule(EnteringKingRule::TryRule), "TryRule");
    }
}
