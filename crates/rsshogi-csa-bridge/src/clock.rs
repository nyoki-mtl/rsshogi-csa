//! The clock ledger.
//!
//! A CSA server's time bookkeeping is deterministic integer arithmetic, and it
//! echoes the consumed time of every move back to both players. That is enough
//! to reconstruct the server's own state exactly rather than estimate it: by the
//! time it is our turn again, both our own move's echo and the opponent's move
//! have arrived, so nothing is in flight and nothing needs to be guessed.
//!
//! The local wall clock answers a different question — the budget for the move
//! currently being searched, whose `,T` value does not exist yet. That is what
//! [`ClockLedger::budget_ms`] is for.
//!
//! Everything here is pure. There is no I/O, no clock reading, and no async
//! runtime type. All durations carry their unit in the name: `_units` means
//! `Time_Unit` units exactly as they appear on the wire, `_ms` means milliseconds.
//!
//! The model is transcribed from `shogi-server`:
//!
//! ```text
//! # time_clock.rb, ChessClock
//! time_duration = [ (end_time - start_time).floor, least_time_per_move ].max
//! timeout?      = (mytime - t + byoyomi + fischer <= 0) &&
//!                 (total_time > 0 || byoyomi > 0 || fischer > 0)
//! process_time  = mytime += fischer; mytime -= t; mytime = 0 if mytime < 0
//!
//! # time_clock.rb, StopWatchClock
//! time_duration = ([floor, least].max / byoyomi) * byoyomi
//! timeout?      = mytime <= t
//! ```
//!
//! `game.rb:264` checks `timeout?` *before* `process_time` runs, so the timeout
//! test always sees the pre-move remaining time.

use std::error::Error;
use std::fmt;

use rsshogi_csa::{Color, GameSummary, TimeConfig, TimeScale, TimeUnit};

/// Which of `shogi-server`'s clock implementations governs a side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClockKind {
    /// `ChessClock`: elapsed time is charged as-is and byoyomi is a per-move allowance.
    Chess,
    /// `StopWatchClock`: elapsed time is truncated to whole byoyomi units.
    StopWatch,
}

/// The time control of one side, normalized for arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockRules {
    /// Which clock implementation applies.
    pub kind: ClockKind,
    /// The wire time unit. Every `_units` value below is in these.
    pub unit: TimeUnit,
    /// `Total_Time`, the starting main time.
    pub total_time_units: u64,
    /// `Byoyomi`, a per-move allowance that is never deducted from the main time.
    pub byoyomi_units: u64,
    /// `Increment`, added to the main time when a move is played.
    pub increment_units: u64,
    /// `Least_Time_Per_Move`, the floor on what a single move costs.
    pub least_time_per_move_units: u64,
    /// `Time_Roundup`: elapsed time rounds up instead of down.
    pub time_roundup: bool,
    /// `Delay`, kept for the record but deliberately not credited. See below.
    pub delay_units: u64,
}

impl ClockRules {
    /// Builds rules from one side's `BEGIN Time` declaration.
    ///
    /// `ClockKind` is inferred, because a `Game_Summary` does not name the clock
    /// implementation. `StopWatchClock` is the only one that reports
    /// `Time_Unit:1min`, so a minute-scaled unit with a non-zero byoyomi is taken
    /// to mean stopwatch; anything else is treated as a chess clock, which is the
    /// safe direction to be wrong in.
    ///
    /// `Delay` is stored but never credited to the move budget. No server this
    /// crate targets emits it, so believing it could only ever overspend.
    #[must_use]
    pub fn from_time_config(config: &TimeConfig) -> Self {
        let unit = config.unit();
        let byoyomi_units = config.byoyomi.unwrap_or(0);
        let kind = if unit.scale == TimeScale::Min && byoyomi_units > 0 {
            ClockKind::StopWatch
        } else {
            ClockKind::Chess
        };
        Self {
            kind,
            unit,
            total_time_units: config.total_time.unwrap_or(0),
            byoyomi_units,
            increment_units: config.increment.unwrap_or(0),
            least_time_per_move_units: config.least_time_per_move.unwrap_or(0),
            time_roundup: config.time_roundup.unwrap_or(false),
            delay_units: config.delay.unwrap_or(0),
        }
    }

    /// Milliseconds in one time unit.
    #[must_use]
    pub const fn unit_ms(&self) -> u64 {
        self.unit.as_millis()
    }

    /// Whether any limit is in force at all.
    ///
    /// `shogi-server` skips the timeout test entirely when total time, byoyomi,
    /// and increment are all zero.
    #[must_use]
    pub const fn is_limited(&self) -> bool {
        self.total_time_units > 0 || self.byoyomi_units > 0 || self.increment_units > 0
    }

    /// What the server would charge for a move that took `wall_ms` of real time.
    ///
    /// This is `time_duration` from `time_clock.rb`.
    #[must_use]
    pub const fn charge_for_units(&self, wall_ms: u64) -> u64 {
        let unit_ms = if self.unit_ms() == 0 { 1 } else { self.unit_ms() };
        let raw = if self.time_roundup {
            wall_ms.saturating_add(unit_ms - 1) / unit_ms
        } else {
            wall_ms / unit_ms
        };
        let charged =
            if raw > self.least_time_per_move_units { raw } else { self.least_time_per_move_units };
        match self.kind {
            ClockKind::Chess => charged,
            ClockKind::StopWatch => match charged.checked_div(self.byoyomi_units) {
                None => charged,
                Some(whole) => whole.saturating_mul(self.byoyomi_units),
            },
        }
    }

    /// The most real time a move may take without being charged more than
    /// `max_charge_units`, or `None` when even an instant move costs too much.
    #[must_use]
    pub const fn max_wall_ms(&self, max_charge_units: u64) -> Option<u64> {
        if self.charge_for_units(0) > max_charge_units {
            return None;
        }
        let unit_ms = if self.unit_ms() == 0 { 1 } else { self.unit_ms() };
        let max_raw = match self.kind {
            ClockKind::Chess => max_charge_units,
            // The charge lands on a multiple of byoyomi, so raw time may run to
            // just under the next multiple.
            ClockKind::StopWatch => match max_charge_units.checked_div(self.byoyomi_units) {
                None => max_charge_units,
                Some(whole) => {
                    whole.saturating_mul(self.byoyomi_units).saturating_add(self.byoyomi_units - 1)
                }
            },
        };
        Some(if self.time_roundup {
            max_raw.saturating_mul(unit_ms)
        } else {
            max_raw.saturating_mul(unit_ms).saturating_add(unit_ms - 1)
        })
    }
}

/// How much time the side to move may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveBudget {
    /// No limit is in force. Total time, byoyomi, and increment are all zero.
    Unlimited,
    /// The largest charge, in time units, that does not lose on time.
    Units(u64),
    /// Every move loses on time.
    Expired,
}

/// USI `go` time parameters derived from the ledger.
///
/// USI forbids `byoyomi` together with `binc`/`winc`, so exactly one of the two
/// forms is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct UsiGoTimes {
    /// `btime`
    pub btime_ms: u64,
    /// `wtime`
    pub wtime_ms: u64,
    /// `byoyomi`, for the side to move.
    pub byoyomi_ms: Option<u64>,
    /// `binc`
    pub binc_ms: Option<u64>,
    /// `winc`
    pub winc_ms: Option<u64>,
}

/// A time declaration this crate cannot compute with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockError {
    /// `Time_Unit` resolved to zero milliseconds, so no conversion is possible.
    ZeroTimeUnit(Color),
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTimeUnit(color) => {
                write!(f, "time unit for `{color}` resolves to zero milliseconds")
            }
        }
    }
}

impl Error for ClockError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SideClock {
    rules: ClockRules,
    remaining_units: u64,
}

/// The reconstructed server-side clock for both players.
///
/// ```
/// use rsshogi_csa_bridge::{ClockLedger, ClockRules, MoveBudget};
/// use rsshogi_csa::{Color, TimeConfig, TimeUnit};
///
/// let control = TimeConfig::new(TimeUnit::SECOND).with_total_time(900).with_increment(5);
/// let rules = ClockRules::from_time_config(&control);
/// let mut ledger = ClockLedger::new(rules.clone(), rules);
///
/// assert_eq!(ledger.remaining_units(Color::Black), 900);
/// // A move charged 11 seconds costs 11 and earns the 5 second increment.
/// ledger.apply_move(Color::Black, 11);
/// assert_eq!(ledger.remaining_units(Color::Black), 894);
/// assert_eq!(ledger.budget(Color::Black), MoveBudget::Units(898));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockLedger {
    black: SideClock,
    white: SideClock,
}

impl ClockLedger {
    /// A ledger at the start of a game, with each side holding its total time.
    #[must_use]
    pub const fn new(black: ClockRules, white: ClockRules) -> Self {
        Self {
            black: SideClock { remaining_units: black.total_time_units, rules: black },
            white: SideClock { remaining_units: white.total_time_units, rules: white },
        }
    }

    /// Builds a ledger from a game proposal.
    ///
    /// `Remaining_Time+` / `Remaining_Time-` win over `Total_Time` when present,
    /// which is how a `%%SHOW` dump or a re-proposed game states where the clocks
    /// already stand. Moves inside `BEGIN Position` are *not* replayed here; feed
    /// them through [`ClockLedger::apply_move`] so that replay and live play use
    /// one code path.
    pub fn from_summary(summary: &GameSummary) -> Result<Self, ClockError> {
        let black = Self::side_from_config(summary.time(Color::Black), Color::Black)?;
        let white = Self::side_from_config(summary.time(Color::White), Color::White)?;
        Ok(Self { black, white })
    }

    fn side_from_config(config: &TimeConfig, color: Color) -> Result<SideClock, ClockError> {
        let rules = ClockRules::from_time_config(config);
        if rules.unit_ms() == 0 {
            return Err(ClockError::ZeroTimeUnit(color));
        }
        let remaining_units = config.remaining_time.unwrap_or(rules.total_time_units);
        Ok(SideClock { rules, remaining_units })
    }

    /// The rules governing one side.
    #[must_use]
    pub const fn rules(&self, color: Color) -> &ClockRules {
        match color {
            Color::Black => &self.black.rules,
            Color::White => &self.white.rules,
        }
    }

    /// Remaining main time, in `Time_Unit` units.
    ///
    /// This mirrors the server's `mytime`, so it excludes byoyomi and does not
    /// pre-credit the increment.
    #[must_use]
    pub const fn remaining_units(&self, color: Color) -> u64 {
        self.side(color).remaining_units
    }

    /// Remaining main time in milliseconds.
    #[must_use]
    pub const fn remaining_ms(&self, color: Color) -> u64 {
        let side = self.side(color);
        side.remaining_units.saturating_mul(side.rules.unit_ms())
    }

    /// Overrides one side's remaining time with a figure the server stated.
    ///
    /// `Remaining_Time+` / `Remaining_Time-` describe the clock *now*, after the
    /// moves in the position block have been played. So a proposal that carries
    /// both must replay first and correct afterwards; using the declared figure
    /// as a starting point and then replaying subtracts the same time twice.
    ///
    /// This touches only the clock. The board is determined by the moves and is
    /// a separate fact, which is why this is not a move.
    pub const fn set_remaining_units(&mut self, color: Color, units: u64) {
        self.side_mut(color).remaining_units = units;
    }

    /// Applies one move echo, charging `charged_units` to `color`.
    ///
    /// This is `process_time`: the increment is added first, the charge is
    /// subtracted, and the result is clamped at zero.
    pub const fn apply_move(&mut self, color: Color, charged_units: u64) {
        let side = self.side_mut(color);
        side.remaining_units = side
            .remaining_units
            .saturating_add(side.rules.increment_units)
            .saturating_sub(charged_units);
    }

    /// Whether a move charged `charged_units` would lose on time.
    ///
    /// This is `timeout?`, evaluated against the pre-move remaining time, exactly
    /// as `game.rb` does.
    #[must_use]
    pub const fn would_time_out(&self, color: Color, charged_units: u64) -> bool {
        let side = self.side(color);
        match side.rules.kind {
            ClockKind::Chess => {
                if !side.rules.is_limited() {
                    return false;
                }
                let pool = side
                    .remaining_units
                    .saturating_add(side.rules.byoyomi_units)
                    .saturating_add(side.rules.increment_units);
                pool <= charged_units
            }
            ClockKind::StopWatch => side.remaining_units <= charged_units,
        }
    }

    /// The largest charge that does not lose on time.
    #[must_use]
    pub const fn budget(&self, color: Color) -> MoveBudget {
        let side = self.side(color);
        let pool = match side.rules.kind {
            ClockKind::Chess => {
                if !side.rules.is_limited() {
                    return MoveBudget::Unlimited;
                }
                side.remaining_units
                    .saturating_add(side.rules.byoyomi_units)
                    .saturating_add(side.rules.increment_units)
            }
            ClockKind::StopWatch => side.remaining_units,
        };
        if pool == 0 {
            return MoveBudget::Expired;
        }
        let max_charge = pool - 1;
        if side.rules.charge_for_units(0) > max_charge {
            return MoveBudget::Expired;
        }
        MoveBudget::Units(max_charge)
    }

    /// How much real time the side to move may spend, in milliseconds.
    ///
    /// `None` means no limit is in force. [`MoveBudget::Expired`] collapses to
    /// `Some(0)`, because a budget of zero and an expired clock are the same
    /// instruction to a search: do not think.
    #[must_use]
    pub const fn budget_ms(&self, color: Color) -> Option<u64> {
        match self.budget(color) {
            MoveBudget::Unlimited => None,
            MoveBudget::Expired => Some(0),
            MoveBudget::Units(units) => match self.side(color).rules.max_wall_ms(units) {
                Some(ms) => Some(ms),
                None => Some(0),
            },
        }
    }

    /// USI `go` parameters for a search by `our` side.
    ///
    /// `margin_ms` is subtracted from our own clock only. The opponent's numbers
    /// stay exact, because an engine allocates its time against them. The margin
    /// comes off the main time first and carries into byoyomi or the increment
    /// when the main time cannot absorb it, which is the case that matters once
    /// the main time is gone.
    ///
    /// Increment and byoyomi are mutually exclusive in USI. Increment wins when
    /// either side declares one; see `decisions.md` for why that direction is safe.
    #[must_use]
    pub const fn usi_times(&self, our: Color, margin_ms: u64) -> UsiGoTimes {
        let mut btime_ms = self.remaining_ms(Color::Black);
        let mut wtime_ms = self.remaining_ms(Color::White);
        let use_increment =
            self.black.rules.increment_units > 0 || self.white.rules.increment_units > 0;

        let mut binc_ms = self.increment_ms(Color::Black);
        let mut winc_ms = self.increment_ms(Color::White);
        let mut byoyomi_ms = self.byoyomi_ms(our);

        {
            let (our_time_ms, our_extra_ms) = match (our, use_increment) {
                (Color::Black, true) => (&mut btime_ms, &mut binc_ms),
                (Color::White, true) => (&mut wtime_ms, &mut winc_ms),
                (Color::Black, false) => (&mut btime_ms, &mut byoyomi_ms),
                (Color::White, false) => (&mut wtime_ms, &mut byoyomi_ms),
            };
            take_margin(our_time_ms, our_extra_ms, margin_ms);
        }

        if use_increment {
            UsiGoTimes {
                btime_ms,
                wtime_ms,
                byoyomi_ms: None,
                binc_ms: Some(binc_ms),
                winc_ms: Some(winc_ms),
            }
        } else if self.rules(our).byoyomi_units > 0 {
            // The form is chosen from what was declared, not from what survives
            // the margin, so a margin that eats the whole byoyomi still reports
            // `byoyomi 0` instead of silently switching to sudden death.
            UsiGoTimes {
                btime_ms,
                wtime_ms,
                byoyomi_ms: Some(byoyomi_ms),
                binc_ms: None,
                winc_ms: None,
            }
        } else {
            UsiGoTimes { btime_ms, wtime_ms, byoyomi_ms: None, binc_ms: None, winc_ms: None }
        }
    }

    const fn byoyomi_ms(&self, color: Color) -> u64 {
        let side = self.side(color);
        side.rules.byoyomi_units.saturating_mul(side.rules.unit_ms())
    }

    const fn increment_ms(&self, color: Color) -> u64 {
        let side = self.side(color);
        side.rules.increment_units.saturating_mul(side.rules.unit_ms())
    }

    const fn side(&self, color: Color) -> &SideClock {
        match color {
            Color::Black => &self.black,
            Color::White => &self.white,
        }
    }

    const fn side_mut(&mut self, color: Color) -> &mut SideClock {
        match color {
            Color::Black => &mut self.black,
            Color::White => &mut self.white,
        }
    }
}

/// Removes `margin_ms` from `time_ms`, spilling any shortfall into `extra_ms`.
const fn take_margin(time_ms: &mut u64, extra_ms: &mut u64, margin_ms: u64) {
    let from_time = if margin_ms < *time_ms { margin_ms } else { *time_ms };
    *time_ms -= from_time;
    let rest = margin_ms - from_time;
    let from_extra = if rest < *extra_ms { rest } else { *extra_ms };
    *extra_ms -= from_extra;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(total: u64, byoyomi: u64, increment: u64) -> TimeConfig {
        TimeConfig::new(TimeUnit::SECOND)
            .with_total_time(total)
            .with_byoyomi(byoyomi)
            .with_increment(increment)
            .with_least_time_per_move(0)
    }

    fn ledger(total: u64, byoyomi: u64, increment: u64) -> ClockLedger {
        let rules = ClockRules::from_time_config(&config(total, byoyomi, increment));
        ClockLedger::new(rules.clone(), rules)
    }

    #[test]
    fn fischer_game_matches_the_server_update_rule() {
        // floodgate-600-10F: 600 seconds plus 10 per move.
        let mut clock = ledger(600, 0, 10);
        assert_eq!(clock.remaining_units(Color::Black), 600);

        clock.apply_move(Color::Black, 11);
        assert_eq!(clock.remaining_units(Color::Black), 599);

        clock.apply_move(Color::Black, 3);
        assert_eq!(clock.remaining_units(Color::Black), 606);

        clock.apply_move(Color::Black, 0);
        assert_eq!(clock.remaining_units(Color::Black), 616);
    }

    #[test]
    fn byoyomi_is_never_deducted_from_the_main_time() {
        // floodgate-900-10 style: byoyomi is a per-move allowance.
        let mut clock = ledger(900, 10, 0);
        clock.apply_move(Color::White, 25);
        assert_eq!(clock.remaining_units(Color::White), 875);
        // The main time is untouched by byoyomi, which only shows up in the budget.
        assert_eq!(clock.budget(Color::White), MoveBudget::Units(884));
    }

    #[test]
    fn remaining_time_clamps_at_zero() {
        let mut clock = ledger(10, 30, 0);
        clock.apply_move(Color::Black, 39);
        assert_eq!(clock.remaining_units(Color::Black), 0);
        // Main time is gone but byoyomi still allows a move.
        assert_eq!(clock.budget(Color::Black), MoveBudget::Units(29));
        assert!(!clock.would_time_out(Color::Black, 29));
        assert!(clock.would_time_out(Color::Black, 30));
    }

    #[test]
    fn budget_is_one_less_than_the_pool() {
        // timeout? is `pool - t <= 0`, so surviving needs t < pool.
        let clock = ledger(0, 30, 0);
        assert_eq!(clock.budget(Color::Black), MoveBudget::Units(29));
        assert!(!clock.would_time_out(Color::Black, 29));
        assert!(clock.would_time_out(Color::Black, 30));
    }

    #[test]
    fn a_fully_drained_clock_is_expired_not_unlimited() {
        let clock = ledger(0, 0, 0);
        // Nothing was declared at all, so the server never tests for timeout.
        assert_eq!(clock.budget(Color::Black), MoveBudget::Unlimited);
        assert!(!clock.would_time_out(Color::Black, 99_999));
        assert_eq!(clock.budget_ms(Color::Black), None);

        // Declared but exhausted is a different state.
        let mut clock = ledger(5, 0, 0);
        clock.apply_move(Color::Black, 5);
        assert_eq!(clock.remaining_units(Color::Black), 0);
        assert_eq!(clock.budget(Color::Black), MoveBudget::Expired);
        assert!(clock.would_time_out(Color::Black, 0));
        assert_eq!(clock.budget_ms(Color::Black), Some(0));
    }

    #[test]
    fn least_time_per_move_sets_a_floor_on_the_charge() {
        let control = config(600, 0, 0).with_least_time_per_move(1);
        let rules = ClockRules::from_time_config(&control);

        assert_eq!(rules.charge_for_units(0), 1);
        assert_eq!(rules.charge_for_units(400), 1);
        assert_eq!(rules.charge_for_units(1_500), 1);
        assert_eq!(rules.charge_for_units(2_100), 2);

        // With only one unit left in the pool the budget is zero, but every move
        // costs at least one, so the position is lost on time either way.
        let clock = ClockLedger::new(rules.clone(), rules);
        let mut clock = clock;
        clock.apply_move(Color::Black, 599);
        assert_eq!(clock.remaining_units(Color::Black), 1);
        assert_eq!(clock.budget(Color::Black), MoveBudget::Expired);
    }

    #[test]
    fn least_time_of_zero_lets_a_fast_move_cost_nothing() {
        // The rule CSA introduced in November 2014.
        let rules = ClockRules::from_time_config(&config(600, 0, 0));
        assert_eq!(rules.charge_for_units(0), 0);
        assert_eq!(rules.charge_for_units(999), 0);
        assert_eq!(rules.charge_for_units(1_000), 1);
    }

    #[test]
    fn wall_clock_budget_accounts_for_flooring() {
        let clock = ledger(0, 30, 0);
        // 29 whole seconds may be charged, so real time may run to 29.999s.
        assert_eq!(clock.budget_ms(Color::Black), Some(29_999));
    }

    #[test]
    fn time_roundup_removes_the_sub_unit_slack() {
        let control = config(0, 30, 0).with_time_roundup(true);
        let rules = ClockRules::from_time_config(&control);

        assert_eq!(rules.charge_for_units(1), 1);
        assert_eq!(rules.charge_for_units(1_000), 1);
        assert_eq!(rules.charge_for_units(1_001), 2);

        let clock = ClockLedger::new(rules.clone(), rules);
        assert_eq!(clock.budget_ms(Color::Black), Some(29_000));
    }

    #[test]
    fn stopwatch_quantizes_the_charge_to_whole_byoyomi_units() {
        let control = TimeConfig::new(TimeUnit { count: 1, scale: TimeScale::Min })
            .with_total_time(25)
            .with_byoyomi(1)
            .with_increment(0)
            .with_least_time_per_move(0);
        let rules = ClockRules::from_time_config(&control);
        assert_eq!(rules.kind, ClockKind::StopWatch);
        assert_eq!(rules.unit_ms(), 60_000);

        // Under a minute is free; a minute costs a minute.
        assert_eq!(rules.charge_for_units(59_999), 0);
        assert_eq!(rules.charge_for_units(60_000), 1);
        assert_eq!(rules.charge_for_units(119_999), 1);

        let clock = ClockLedger::new(rules.clone(), rules);
        // timeout? is `mytime <= t`, so 24 minutes may be charged out of 25.
        assert_eq!(clock.budget(Color::Black), MoveBudget::Units(24));
        assert_eq!(clock.budget_ms(Color::Black), Some(24 * 60_000 + 59_999));
    }

    #[test]
    fn stopwatch_with_a_coarse_byoyomi_rounds_the_budget_down() {
        let control = TimeConfig::new(TimeUnit { count: 1, scale: TimeScale::Min })
            .with_total_time(25)
            .with_byoyomi(10)
            .with_least_time_per_move(0);
        let rules = ClockRules::from_time_config(&control);
        // A charge lands on a multiple of ten, so 24 allows a raw 29 minutes.
        assert_eq!(rules.charge_for_units(29 * 60_000), 20);
        assert_eq!(rules.max_wall_ms(24), Some(29 * 60_000 + 59_999));
    }

    #[test]
    fn usi_times_use_increment_form_when_increment_is_declared() {
        let clock = ledger(600, 0, 10);
        let times = clock.usi_times(Color::Black, 0);
        assert_eq!(times.btime_ms, 600_000);
        assert_eq!(times.wtime_ms, 600_000);
        assert_eq!(times.binc_ms, Some(10_000));
        assert_eq!(times.winc_ms, Some(10_000));
        assert_eq!(times.byoyomi_ms, None);
    }

    #[test]
    fn usi_times_use_byoyomi_form_when_no_increment_is_declared() {
        let clock = ledger(900, 10, 0);
        let times = clock.usi_times(Color::White, 0);
        assert_eq!(times.byoyomi_ms, Some(10_000));
        assert_eq!(times.binc_ms, None);
        assert_eq!(times.winc_ms, None);
    }

    #[test]
    fn usi_times_omit_both_forms_for_a_sudden_death_game() {
        let clock = ledger(1_500, 0, 0);
        let times = clock.usi_times(Color::Black, 0);
        assert_eq!(times.byoyomi_ms, None);
        assert_eq!(times.binc_ms, None);
        assert_eq!(times.winc_ms, None);
        assert_eq!(times.btime_ms, 1_500_000);
    }

    #[test]
    fn margin_comes_off_our_clock_only() {
        let clock = ledger(900, 10, 0);
        let times = clock.usi_times(Color::Black, 2_500);
        assert_eq!(times.btime_ms, 897_500);
        assert_eq!(times.wtime_ms, 900_000, "the opponent's clock stays exact");
        assert_eq!(times.byoyomi_ms, Some(10_000));
    }

    #[test]
    fn margin_carries_into_byoyomi_when_the_main_time_is_gone() {
        let mut clock = ledger(30, 10, 0);
        clock.apply_move(Color::Black, 31);
        assert_eq!(clock.remaining_ms(Color::Black), 0);

        let times = clock.usi_times(Color::Black, 2_500);
        assert_eq!(times.btime_ms, 0);
        assert_eq!(times.byoyomi_ms, Some(7_500));
    }

    #[test]
    fn margin_carries_partially_when_the_main_time_is_short() {
        let mut clock = ledger(30, 10, 0);
        clock.apply_move(Color::Black, 29);
        assert_eq!(clock.remaining_ms(Color::Black), 1_000);

        let times = clock.usi_times(Color::Black, 2_500);
        assert_eq!(times.btime_ms, 0);
        assert_eq!(times.byoyomi_ms, Some(8_500));
    }

    #[test]
    fn margin_carries_into_the_increment_for_a_fischer_game() {
        let mut clock = ledger(10, 0, 10);
        clock.apply_move(Color::White, 20);
        assert_eq!(clock.remaining_ms(Color::White), 0);

        let times = clock.usi_times(Color::White, 2_500);
        assert_eq!(times.wtime_ms, 0);
        assert_eq!(times.winc_ms, Some(7_500));
        assert_eq!(times.binc_ms, Some(10_000), "the opponent's increment stays exact");
    }

    #[test]
    fn margin_cannot_drive_a_clock_below_zero() {
        let mut clock = ledger(5, 1, 0);
        clock.apply_move(Color::Black, 5);
        let times = clock.usi_times(Color::Black, 60_000);
        assert_eq!(times.btime_ms, 0);
        // The byoyomi form is kept because byoyomi was declared, even though the
        // margin ate all of it. Dropping to sudden death here would be a lie.
        assert_eq!(times.byoyomi_ms, Some(0));

        let times = clock.usi_times(Color::Black, 500);
        assert_eq!(times.byoyomi_ms, Some(500));
    }

    #[test]
    fn per_side_time_controls_stay_independent() {
        let black = ClockRules::from_time_config(&config(600, 0, 0));
        let white = ClockRules::from_time_config(&config(900, 0, 0));
        let mut clock = ClockLedger::new(black, white);

        clock.apply_move(Color::Black, 10);
        assert_eq!(clock.remaining_units(Color::Black), 590);
        assert_eq!(clock.remaining_units(Color::White), 900);
        assert_eq!(clock.rules(Color::White).total_time_units, 900);
    }
}
