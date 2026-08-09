//! Canonical formatting for the CSA server protocol.
//!
//! Every command has exactly one canonical spelling. Fields the parser kept
//! verbatim, such as the remainder of an `##[TAG]` line or the arguments of an
//! unrecognized `%%` command, are emitted unchanged so unknown dialects survive
//! a round trip.

use std::fmt::Write as _;

use crate::model::{CsaCommand, MetaCommand, MoveLine};

/// Formats one command as its canonical protocol line, without a trailing newline.
///
/// ```
/// use rsshogi_csa::{format_command, parse_line};
///
/// let command = parse_line("+7776FU,T12").unwrap();
/// assert_eq!(format_command(&command), "+7776FU,T12");
/// ```
#[must_use]
pub fn format_command(command: &CsaCommand) -> String {
    match command {
        CsaCommand::Login { name, password, x1 } => {
            let suffix = if *x1 { " x1" } else { "" };
            format!("LOGIN {name} {password}{suffix}")
        }
        CsaCommand::Logout => "LOGOUT".to_owned(),
        CsaCommand::Agree { game_id } => format_with_optional_id("AGREE", game_id.as_deref()),
        CsaCommand::Reject { game_id } => format_with_optional_id("REJECT", game_id.as_deref()),
        CsaCommand::Challenge => "CHALLENGE".to_owned(),
        CsaCommand::Meta(meta) => format_meta(meta),

        CsaCommand::LoginOk { name } => format!("LOGIN:{name} OK"),
        CsaCommand::LoginIncorrect => "LOGIN:incorrect".to_owned(),
        CsaCommand::LogoutCompleted => "LOGOUT:completed".to_owned(),
        CsaCommand::ChallengeAccepted => "CHALLENGE ACCEPTED".to_owned(),
        CsaCommand::BeginGameSummary => "BEGIN Game_Summary".to_owned(),
        CsaCommand::Start { game_id } => format!("START:{game_id}"),
        CsaCommand::RejectNotice { game_id, by } => format!("REJECT:{game_id} by {by}"),
        CsaCommand::End(marker) => format!("#{}", marker.as_token()),
        CsaCommand::Info(info) => format!("##[{}]{}", info.tag, info.rest),

        CsaCommand::Move(line) => format_move(line),
        CsaCommand::Special { kind, elapsed } => {
            let mut out = format!("%{}", kind.as_token());
            if let Some(elapsed) = elapsed {
                let _ = write!(out, ",T{elapsed}");
            }
            out
        }
        CsaCommand::KeepAlive => String::new(),
    }
}

fn format_with_optional_id(keyword: &str, game_id: Option<&str>) -> String {
    game_id.map_or_else(|| keyword.to_owned(), |id| format!("{keyword} {id}"))
}

fn format_move(line: &MoveLine) -> String {
    let mut out = line.csa.as_str().to_owned();
    if let Some(elapsed) = line.elapsed {
        let _ = write!(out, ",T{elapsed}");
    }
    if let Some(comment) = &line.comment {
        let _ = write!(out, ",'{comment}");
    }
    out
}

fn format_meta(meta: &MetaCommand) -> String {
    match meta {
        MetaCommand::Game { name, turn } => {
            let mut out = "%%GAME".to_owned();
            if let Some(name) = name {
                let _ = write!(out, " {name}");
            }
            if let Some(turn) = turn {
                let _ = write!(out, " {}", turn.as_csa());
            }
            out
        }
        MetaCommand::Challenge { name, turn } => {
            format!("%%CHALLENGE {name} {}", turn.as_csa())
        }
        MetaCommand::Who => "%%WHO".to_owned(),
        MetaCommand::List => "%%LIST".to_owned(),
        MetaCommand::Show(game_id) => format!("%%SHOW {game_id}"),
        MetaCommand::Chat(message) => format!("%%CHAT {message}"),
        MetaCommand::Rating => "%%RATING".to_owned(),
        MetaCommand::Version => "%%VERSION".to_owned(),
        MetaCommand::Help => "%%HELP".to_owned(),
        MetaCommand::Other { name, args } => {
            let mut out = format!("%%{name}");
            for arg in args {
                let _ = write!(out, " {arg}");
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CsaMove, EndMarker, InfoLine, SpecialMove, TurnPreference};
    use crate::parse_line;

    fn roundtrip(line: &str) {
        let command = parse_line(line).unwrap_or_else(|err| panic!("`{line}` should parse: {err}"));
        assert_eq!(format_command(&command), line);
    }

    #[test]
    fn canonical_lines_survive_a_round_trip() {
        for line in [
            "LOGIN engine secret",
            "LOGIN engine floodgate-600-10,secret x1",
            "LOGOUT",
            "AGREE",
            "AGREE game-1",
            "REJECT",
            "REJECT game-1",
            "CHALLENGE",
            "CHALLENGE ACCEPTED",
            "LOGIN:engine OK",
            "LOGIN:incorrect",
            "LOGOUT:completed",
            "BEGIN Game_Summary",
            "START:wdoor+floodgate-900-0+a+b+20260804120000",
            "REJECT:game-1 by the Server (timed out)",
            "#RESIGN",
            "#WIN",
            "#OUTE_SENNICHITE",
            "##[ERROR] unknown command: foo",
            "##[LIST] +OK",
            "##[FORK]: new buoy game name: buoy_x-900-0",
            "+7776FU",
            "+7776FU,T12",
            "-8384FU,'* 60 +2625FU -8485FU",
            "-8384FU,T7,'* 60 +2625FU",
            "%TORYO",
            "%KACHI,T20",
            "%+ILLEGAL_ACTION",
            "%%GAME",
            "%%GAME floodgate-900-0 *",
            "%%CHALLENGE test-1500-0 +",
            "%%WHO",
            "%%LIST",
            "%%SHOW game-1",
            "%%CHAT hello world",
            "%%RATING",
            "%%VERSION",
            "%%HELP",
            "%%SETBUOY buoy_x-900-0 +7776FUT2 3",
        ] {
            roundtrip(line);
        }
    }

    #[test]
    fn keepalive_formats_as_an_empty_line() {
        assert_eq!(format_command(&CsaCommand::KeepAlive), "");
    }

    #[test]
    fn constructed_commands_format_without_a_parse_step() {
        let mv = CsaMove::parse("+7776FU").expect("valid move");
        assert_eq!(
            format_command(&CsaCommand::Move(MoveLine {
                csa: mv,
                elapsed: Some(3),
                comment: Some("* 120".to_owned()),
            })),
            "+7776FU,T3,'* 120"
        );
        assert_eq!(
            format_command(&CsaCommand::Special { kind: SpecialMove::Toryo, elapsed: None }),
            "%TORYO"
        );
        assert_eq!(format_command(&CsaCommand::End(EndMarker::Censored)), "#CENSORED");
        assert_eq!(
            format_command(&CsaCommand::Info(InfoLine {
                tag: "WHO".to_owned(),
                rest: " +OK".to_owned()
            })),
            "##[WHO] +OK"
        );
        assert_eq!(
            format_command(&CsaCommand::Meta(MetaCommand::Game {
                name: Some("test-1500-0".to_owned()),
                turn: Some(TurnPreference::White),
            })),
            "%%GAME test-1500-0 -"
        );
    }
}
