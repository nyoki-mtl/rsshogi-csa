//! Reading and writing the `BEGIN Game_Summary` block.
//!
//! The block is not a single line, so it does not go through
//! [`parse_line`](crate::parse_line). Feed its lines to a [`GameSummaryReader`]
//! instead, starting with `BEGIN Game_Summary` itself. The reader is a nested
//! state machine: `BEGIN Time`, `BEGIN Time+`, `BEGIN Time-`, and
//! `BEGIN Position` each push a sub-state, which is what lets the position body
//! be captured verbatim instead of being mistaken for commands.

use std::mem;

use crate::error::{ParseError, ParseErrorKind};
use crate::model::{Color, GameSummary, TimeConfig, TimeScale, TimeUnit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitBegin,
    Summary,
    Time(Option<Color>),
    Position,
    Done,
}

/// Assembles a [`GameSummary`] from the lines of one `BEGIN Game_Summary` block.
///
/// ```
/// use rsshogi_csa::GameSummaryReader;
///
/// let block = "\
/// BEGIN Game_Summary
/// Protocol_Version:1.2
/// Your_Turn:+
/// BEGIN Time
/// Time_Unit:1sec
/// Total_Time:900
/// END Time
/// BEGIN Position
/// PI
/// +
/// END Position
/// END Game_Summary";
///
/// let mut reader = GameSummaryReader::new();
/// let mut summary = None;
/// for line in block.lines() {
///     if let Some(done) = reader.feed(line).unwrap() {
///         summary = Some(done);
///     }
/// }
///
/// let summary = summary.expect("block is complete");
/// assert_eq!(summary.time_black.total_time, Some(900));
/// assert_eq!(summary.position_lines, vec!["PI".to_string(), "+".to_string()]);
/// ```
#[derive(Debug, Clone)]
pub struct GameSummaryReader {
    state: State,
    summary: GameSummary,
}

impl GameSummaryReader {
    /// A reader waiting for `BEGIN Game_Summary`.
    #[must_use]
    pub fn new() -> Self {
        Self { state: State::AwaitBegin, summary: GameSummary::default() }
    }

    /// Whether the block has been closed by `END Game_Summary`.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// Feeds one line, returning the summary once `END Game_Summary` arrives.
    ///
    /// Lines inside `BEGIN Position` are stored verbatim apart from trailing
    /// newline characters, because board rows carry significant trailing spaces.
    pub fn feed(&mut self, raw: &str) -> Result<Option<GameSummary>, ParseError> {
        let line = raw.trim_end_matches(['\r', '\n']);
        match self.state {
            State::AwaitBegin => {
                if line.trim() == "BEGIN Game_Summary" {
                    self.state = State::Summary;
                    Ok(None)
                } else {
                    Err(ParseError::new(ParseErrorKind::UnexpectedLine, line))
                }
            }
            State::Summary => self.feed_summary(line),
            State::Time(color) => self.feed_time(line, color),
            State::Position => {
                if line.trim() == "END Position" {
                    self.state = State::Summary;
                } else if line.trim() == "END Game_Summary" {
                    return Err(ParseError::new(ParseErrorKind::UnexpectedLine, line));
                } else {
                    self.summary.position_lines.push(line.to_owned());
                }
                Ok(None)
            }
            State::Done => Err(ParseError::new(ParseErrorKind::UnexpectedLine, line)),
        }
    }

    fn feed_summary(&mut self, line: &str) -> Result<Option<GameSummary>, ParseError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        match trimmed {
            "END Game_Summary" => {
                self.state = State::Done;
                return Ok(Some(mem::take(&mut self.summary)));
            }
            "BEGIN Time" => self.state = State::Time(None),
            "BEGIN Time+" => self.state = State::Time(Some(Color::Black)),
            "BEGIN Time-" => self.state = State::Time(Some(Color::White)),
            "BEGIN Position" => self.state = State::Position,
            _ => {
                let (key, value) = split_field(trimmed)?;
                self.set_summary_field(trimmed, key, value)?;
            }
        }
        Ok(None)
    }

    fn feed_time(
        &mut self,
        line: &str,
        color: Option<Color>,
    ) -> Result<Option<GameSummary>, ParseError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        if trimmed == time_terminator(color) {
            self.state = State::Summary;
            return Ok(None);
        }
        let (key, value) = split_field(trimmed)?;
        self.set_time_field(trimmed, key, value, color)?;
        Ok(None)
    }

    fn set_summary_field(&mut self, line: &str, key: &str, value: &str) -> Result<(), ParseError> {
        let summary = &mut self.summary;
        match key {
            "Protocol_Version" => summary.protocol_version = Some(value.to_owned()),
            "Protocol_Mode" => summary.protocol_mode = Some(value.to_owned()),
            "Format" => summary.format = Some(value.to_owned()),
            "Declaration" => summary.declaration = Some(value.to_owned()),
            "Game_ID" => summary.game_id = Some(value.to_owned()),
            "Name+" => summary.name_black = Some(value.to_owned()),
            "Name-" => summary.name_white = Some(value.to_owned()),
            "Your_Turn" => summary.your_turn = Some(parse_color(line, "Your_Turn", value)?),
            "To_Move" => summary.to_move = Some(parse_color(line, "To_Move", value)?),
            "Rematch_On_Draw" => summary.rematch_on_draw = Some(value.to_owned()),
            "Max_Moves" => summary.max_moves = Some(parse_u32(line, "Max_Moves", value)?),
            _ => summary.extras.push((key.to_owned(), value.to_owned())),
        }
        Ok(())
    }

    fn set_time_field(
        &mut self,
        line: &str,
        key: &str,
        value: &str,
        color: Option<Color>,
    ) -> Result<(), ParseError> {
        match key {
            "Time_Unit" => {
                let unit = parse_time_unit(line, value)?;
                self.apply_time(color, |config| config.time_unit = Some(unit));
            }
            "Total_Time" => {
                let parsed = parse_u64(line, "Total_Time", value)?;
                self.apply_time(color, |config| config.total_time = Some(parsed));
            }
            "Byoyomi" => {
                let parsed = parse_u64(line, "Byoyomi", value)?;
                self.apply_time(color, |config| config.byoyomi = Some(parsed));
            }
            "Delay" => {
                let parsed = parse_u64(line, "Delay", value)?;
                self.apply_time(color, |config| config.delay = Some(parsed));
            }
            "Increment" => {
                let parsed = parse_u64(line, "Increment", value)?;
                self.apply_time(color, |config| config.increment = Some(parsed));
            }
            "Least_Time_Per_Move" => {
                let parsed = parse_u64(line, "Least_Time_Per_Move", value)?;
                self.apply_time(color, |config| config.least_time_per_move = Some(parsed));
            }
            "Time_Roundup" => {
                let parsed = parse_yes_no(line, "Time_Roundup", value)?;
                self.apply_time(color, |config| config.time_roundup = Some(parsed));
            }
            "Remaining_Time" => {
                let parsed = parse_u64(line, "Remaining_Time", value)?;
                self.apply_time(color, |config| config.remaining_time = Some(parsed));
            }
            "Remaining_Time+" => {
                let parsed = parse_u64(line, "Remaining_Time+", value)?;
                self.summary.time_black.remaining_time = Some(parsed);
            }
            "Remaining_Time-" => {
                let parsed = parse_u64(line, "Remaining_Time-", value)?;
                self.summary.time_white.remaining_time = Some(parsed);
            }
            "Last_Move" => self.summary.last_move = Some(value.to_owned()),
            "Current_Turn" => {
                self.summary.current_turn = Some(parse_u32(line, "Current_Turn", value)?);
            }
            _ => {
                let entry = (key.to_owned(), value.to_owned());
                self.apply_time(color, |config| config.extras.push(entry.clone()));
            }
        }
        Ok(())
    }

    fn apply_time(&mut self, color: Option<Color>, mut apply: impl FnMut(&mut TimeConfig)) {
        match color {
            Some(Color::Black) => apply(&mut self.summary.time_black),
            Some(Color::White) => apply(&mut self.summary.time_white),
            None => {
                apply(&mut self.summary.time_black);
                apply(&mut self.summary.time_white);
            }
        }
    }
}

const fn time_terminator(color: Option<Color>) -> &'static str {
    match color {
        None => "END Time",
        Some(Color::Black) => "END Time+",
        Some(Color::White) => "END Time-",
    }
}

impl Default for GameSummaryReader {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeConfig {
    /// Whether no field of this time control was declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn push_body(&self, out: &mut Vec<String>) {
        push_field(out, "Time_Unit", self.time_unit.map(|unit| unit.to_string()));
        push_field(out, "Total_Time", self.total_time.map(|v| v.to_string()));
        push_field(out, "Byoyomi", self.byoyomi.map(|v| v.to_string()));
        push_field(out, "Delay", self.delay.map(|v| v.to_string()));
        push_field(out, "Increment", self.increment.map(|v| v.to_string()));
        push_field(out, "Least_Time_Per_Move", self.least_time_per_move.map(|v| v.to_string()));
        push_field(
            out,
            "Time_Roundup",
            self.time_roundup.map(|v| if v { "YES" } else { "NO" }.to_owned()),
        );
        for (key, value) in &self.extras {
            out.push(format!("{key}:{value}"));
        }
    }
}

impl GameSummary {
    /// Renders the summary as protocol lines, without trailing newlines.
    ///
    /// Field order follows `shogi-server`, so a proposal parsed from it renders
    /// back byte for byte. A block that declared identical per-color time
    /// sections is re-emitted as one shared `BEGIN Time` section.
    #[must_use]
    pub fn to_lines(&self) -> Vec<String> {
        let mut out = vec!["BEGIN Game_Summary".to_owned()];
        push_field(&mut out, "Protocol_Version", self.protocol_version.clone());
        push_field(&mut out, "Protocol_Mode", self.protocol_mode.clone());
        push_field(&mut out, "Format", self.format.clone());
        push_field(&mut out, "Declaration", self.declaration.clone());
        push_field(&mut out, "Game_ID", self.game_id.clone());
        push_field(&mut out, "Name+", self.name_black.clone());
        push_field(&mut out, "Name-", self.name_white.clone());
        push_field(&mut out, "Your_Turn", self.your_turn.map(|c| c.to_string()));
        push_field(&mut out, "Rematch_On_Draw", self.rematch_on_draw.clone());
        push_field(&mut out, "To_Move", self.to_move.map(|c| c.to_string()));
        push_field(&mut out, "Max_Moves", self.max_moves.map(|v| v.to_string()));
        for (key, value) in &self.extras {
            out.push(format!("{key}:{value}"));
        }

        self.push_time_section(&mut out);

        if !self.position_lines.is_empty() {
            out.push("BEGIN Position".to_owned());
            out.extend(self.position_lines.iter().cloned());
            out.push("END Position".to_owned());
        }

        out.push("END Game_Summary".to_owned());
        out
    }

    fn push_time_section(&self, out: &mut Vec<String>) {
        if self.time_black.is_empty() && self.time_white.is_empty() {
            return;
        }

        if self.time_black.eq_ignoring_remaining(&self.time_white) {
            out.push("BEGIN Time".to_owned());
            self.time_black.push_body(out);
            push_field(
                out,
                "Remaining_Time+",
                self.time_black.remaining_time.map(|v| v.to_string()),
            );
            push_field(
                out,
                "Remaining_Time-",
                self.time_white.remaining_time.map(|v| v.to_string()),
            );
            self.push_show_fields(out);
            out.push("END Time".to_owned());
            return;
        }

        out.push("BEGIN Time+".to_owned());
        self.time_black.push_body(out);
        push_field(out, "Remaining_Time+", self.time_black.remaining_time.map(|v| v.to_string()));
        self.push_show_fields(out);
        out.push("END Time+".to_owned());

        out.push("BEGIN Time-".to_owned());
        self.time_white.push_body(out);
        push_field(out, "Remaining_Time-", self.time_white.remaining_time.map(|v| v.to_string()));
        out.push("END Time-".to_owned());
    }

    fn push_show_fields(&self, out: &mut Vec<String>) {
        push_field(out, "Last_Move", self.last_move.clone());
        push_field(out, "Current_Turn", self.current_turn.map(|v| v.to_string()));
    }
}

fn push_field(out: &mut Vec<String>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        out.push(format!("{key}:{value}"));
    }
}

fn split_field(line: &str) -> Result<(&str, &str), ParseError> {
    line.split_once(':').ok_or_else(|| ParseError::new(ParseErrorKind::UnexpectedLine, line))
}

fn parse_color(line: &str, key: &'static str, value: &str) -> Result<Color, ParseError> {
    let mut chars = value.chars();
    let (Some(first), None) = (chars.next(), chars.next()) else {
        return Err(ParseError::new(ParseErrorKind::InvalidField(key), line));
    };
    Color::from_csa(first).ok_or_else(|| ParseError::new(ParseErrorKind::InvalidField(key), line))
}

fn parse_u64(line: &str, key: &'static str, value: &str) -> Result<u64, ParseError> {
    value.parse().map_err(|_| ParseError::new(ParseErrorKind::InvalidNumber(key), line))
}

fn parse_u32(line: &str, key: &'static str, value: &str) -> Result<u32, ParseError> {
    value.parse().map_err(|_| ParseError::new(ParseErrorKind::InvalidNumber(key), line))
}

fn parse_yes_no(line: &str, key: &'static str, value: &str) -> Result<bool, ParseError> {
    match value.to_ascii_uppercase().as_str() {
        "YES" | "TRUE" | "1" => Ok(true),
        "NO" | "FALSE" | "0" => Ok(false),
        _ => Err(ParseError::new(ParseErrorKind::InvalidField(key), line)),
    }
}

fn parse_time_unit(line: &str, value: &str) -> Result<TimeUnit, ParseError> {
    const SCALES: [(&str, TimeScale); 3] =
        [("msec", TimeScale::Msec), ("sec", TimeScale::Sec), ("min", TimeScale::Min)];

    for (suffix, scale) in SCALES {
        let Some(head) = value.strip_suffix(suffix) else {
            continue;
        };
        let count = if head.is_empty() {
            1
        } else {
            head.parse()
                .map_err(|_| ParseError::new(ParseErrorKind::InvalidNumber("Time_Unit"), line))?
        };
        return Ok(TimeUnit { count, scale });
    }
    Err(ParseError::new(ParseErrorKind::InvalidField("Time_Unit"), line))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPOSAL: &str = "\
BEGIN Game_Summary
Protocol_Version:1.2
Protocol_Mode:Server
Format:Shogi 1.0
Declaration:Jishogi 1.1
Game_ID:wdoor+floodgate-900-0+black+white+20260804120000
Name+:black
Name-:white
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
P1-KY-KE-GI-KI-OU-KI-GI-KE-KY
P9+KY+KE+GI+KI+OU+KI+GI+KE+KY
+
END Position
END Game_Summary";

    fn read(block: &str) -> GameSummary {
        let mut reader = GameSummaryReader::new();
        let mut result = None;
        for line in block.lines() {
            let done = reader
                .feed(line)
                .unwrap_or_else(|err| panic!("`{line}` should be accepted: {err}"));
            if let Some(summary) = done {
                result = Some(summary);
            }
        }
        assert!(reader.is_complete(), "reader should reach END Game_Summary");
        result.expect("block should produce a summary")
    }

    #[test]
    fn proposal_round_trips_byte_for_byte() {
        let summary = read(PROPOSAL);
        assert_eq!(summary.to_lines().join("\n"), PROPOSAL);
    }

    #[test]
    fn proposal_fields_are_typed() {
        let summary = read(PROPOSAL);
        assert_eq!(summary.protocol_version.as_deref(), Some("1.2"));
        assert_eq!(summary.your_turn, Some(Color::Black));
        assert_eq!(summary.to_move, Some(Color::Black));
        assert_eq!(summary.max_moves, Some(256));
        assert_eq!(summary.name_black.as_deref(), Some("black"));
        assert_eq!(summary.time_black.unit(), TimeUnit::SECOND);
        assert_eq!(summary.time_black.total_time, Some(900));
        assert_eq!(summary.time_white.total_time, Some(900));
        assert_eq!(summary.position_lines.len(), 3);
    }

    #[test]
    fn position_body_keeps_significant_trailing_spaces() {
        // The board row is built here rather than written inline, because a
        // trailing space in a source literal does not survive editors or hooks.
        let row = format!("P2 * -HI *  *  *  *  * -KA *{}", ' ');
        let block =
            format!("BEGIN Game_Summary\nBEGIN Position\n{row}\n+\nEND Position\nEND Game_Summary");
        let summary = read(&block);
        assert_eq!(summary.position_lines[0], row);
        assert_eq!(summary.to_lines().join("\n"), block);
    }

    #[test]
    fn per_color_time_blocks_are_kept_apart() {
        let block = "\
BEGIN Game_Summary
BEGIN Time+
Time_Unit:1sec
Total_Time:600
END Time+
BEGIN Time-
Time_Unit:1sec
Total_Time:900
END Time-
END Game_Summary";
        let summary = read(block);
        assert_eq!(summary.time_black.total_time, Some(600));
        assert_eq!(summary.time_white.total_time, Some(900));
        assert_eq!(summary.to_lines().join("\n"), block);
    }

    #[test]
    fn time_blocks_require_matching_terminators() {
        for (begin, end) in
            [("BEGIN Time", "END Time+"), ("BEGIN Time+", "END Time-"), ("BEGIN Time-", "END Time")]
        {
            let mut reader = GameSummaryReader::new();
            reader.feed("BEGIN Game_Summary").expect("begin summary");
            reader.feed(begin).expect("begin time block");
            assert!(reader.feed(end).is_err(), "{begin} must reject {end}");
        }

        for (begin, end, black, white) in [
            ("BEGIN Time", "END Time", Some(60), Some(60)),
            ("BEGIN Time+", "END Time+", Some(60), None),
            ("BEGIN Time-", "END Time-", None, Some(60)),
        ] {
            let summary = read(&format!(
                "BEGIN Game_Summary\n{begin}\nTotal_Time:60\n{end}\nEND Game_Summary"
            ));
            assert_eq!(summary.time_black.total_time, black);
            assert_eq!(summary.time_white.total_time, white);
        }
    }

    #[test]
    fn position_block_must_end_before_the_summary() {
        let mut reader = GameSummaryReader::new();
        reader.feed("BEGIN Game_Summary").expect("begin summary");
        reader.feed("BEGIN Position").expect("begin position");
        reader.feed("PI").expect("position line");
        assert!(reader.feed("END Game_Summary").is_err());
    }

    #[test]
    fn show_output_round_trips() {
        let block = "\
BEGIN Game_Summary
Protocol_Version:1.2
Game_ID:game-1
Rematch_On_Draw:NO
To_Move:+
Max_Moves:256
BEGIN Time
Time_Unit:1sec
Total_Time:1500
Byoyomi:0
Increment:0
Least_Time_Per_Move:0
Remaining_Time+:1400
Remaining_Time-:1450
Last_Move:+7776FU,T12
Current_Turn:2
END Time
END Game_Summary";
        let summary = read(block);
        assert_eq!(summary.time_black.remaining_time, Some(1400));
        assert_eq!(summary.time_white.remaining_time, Some(1450));
        assert_eq!(summary.last_move.as_deref(), Some("+7776FU,T12"));
        assert_eq!(summary.current_turn, Some(2));
        assert_eq!(summary.to_lines().join("\n"), block);
    }

    #[test]
    fn resumed_position_carries_moves() {
        let block = "\
BEGIN Game_Summary
Your_Turn:-
BEGIN Position
PI
+
+7776FU,T2
-3334FU,T5
END Position
END Game_Summary";
        let summary = read(block);
        let (board, moves) = summary.position_split();
        assert_eq!(board, ["PI".to_owned(), "+".to_owned()]);
        assert_eq!(moves, ["+7776FU,T2".to_owned(), "-3334FU,T5".to_owned()]);
    }

    #[test]
    fn unknown_fields_are_preserved_in_order() {
        let block = "\
BEGIN Game_Summary
Protocol_Version:1.2
Future_Field:whatever
BEGIN Time
Total_Time:60
Future_Time_Field:7
END Time
END Game_Summary";
        let summary = read(block);
        assert_eq!(summary.extras, [("Future_Field".to_owned(), "whatever".to_owned())]);
        assert_eq!(summary.time_black.extras, [("Future_Time_Field".to_owned(), "7".to_owned())]);
        assert_eq!(summary.to_lines().join("\n"), block);
    }

    #[test]
    fn time_unit_variants() {
        assert_eq!(
            parse_time_unit("Time_Unit:1sec", "1sec").expect("1sec"),
            TimeUnit { count: 1, scale: TimeScale::Sec }
        );
        assert_eq!(
            parse_time_unit("Time_Unit:1min", "1min").expect("1min"),
            TimeUnit { count: 1, scale: TimeScale::Min }
        );
        assert_eq!(
            parse_time_unit("Time_Unit:500msec", "500msec").expect("500msec"),
            TimeUnit { count: 500, scale: TimeScale::Msec }
        );
        assert!(parse_time_unit("Time_Unit:1hour", "1hour").is_err());
        assert!(parse_time_unit("Time_Unit:xsec", "xsec").is_err());
    }

    #[test]
    fn reader_rejects_lines_outside_the_block() {
        let mut reader = GameSummaryReader::new();
        assert!(reader.feed("Protocol_Version:1.2").is_err());

        let mut reader = GameSummaryReader::new();
        reader.feed("BEGIN Game_Summary").expect("begin");
        assert!(reader.feed("not a field").is_err());
    }

    #[test]
    fn reader_rejects_lines_after_the_block() {
        let mut reader = GameSummaryReader::new();
        reader.feed("BEGIN Game_Summary").expect("begin");
        reader.feed("END Game_Summary").expect("end");
        assert!(reader.feed("Protocol_Version:1.2").is_err());
    }

    #[test]
    fn malformed_numbers_are_reported() {
        let mut reader = GameSummaryReader::new();
        reader.feed("BEGIN Game_Summary").expect("begin");
        assert!(reader.feed("Max_Moves:many").is_err());
        assert!(reader.feed("Your_Turn:?").is_err());
    }
}
