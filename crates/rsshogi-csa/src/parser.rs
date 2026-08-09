//! Line parsing for the CSA server protocol.
//!
//! [`parse_line`] is permissive: unknown `%%` commands, unknown `%` special
//! moves, and unknown `#` markers are preserved rather than rejected, so a
//! server dialect this crate has not seen still round-trips.
//!
//! [`parse_line_strict`] additionally requires the input to already be in
//! canonical form, which is what contract tests want.
//!
//! Neither function understands the body of a `BEGIN Position` block. Board
//! rows and the side-to-move line are not commands, and a replayed move inside
//! the block is indistinguishable from a live move without the surrounding
//! state. Feed the whole `BEGIN Game_Summary` block to
//! [`GameSummaryReader`](crate::GameSummaryReader) instead.

use crate::error::{ParseError, ParseErrorKind};
use crate::format::format_command;
use crate::model::{
    CsaCommand, CsaMove, EndMarker, InfoLine, MetaCommand, MoveLine, SpecialMove, TurnPreference,
};

/// Parses one protocol line.
///
/// Leading and trailing whitespace, including a trailing `\r`, is ignored. An
/// empty line is the application-level keepalive.
///
/// ```
/// use rsshogi_csa::{parse_line, CsaCommand, EndMarker};
///
/// assert_eq!(parse_line("#RESIGN").unwrap(), CsaCommand::End(EndMarker::Resign));
/// assert_eq!(parse_line("").unwrap(), CsaCommand::KeepAlive);
/// ```
pub fn parse_line(raw: &str) -> Result<CsaCommand, ParseError> {
    let line = raw.trim();
    if line.is_empty() {
        return Ok(CsaCommand::KeepAlive);
    }

    if let Some(rest) = line.strip_prefix("##[") {
        return parse_info(line, rest);
    }
    if let Some(rest) = line.strip_prefix("%%") {
        return parse_meta(line, rest);
    }
    if let Some(rest) = line.strip_prefix('%') {
        return parse_special(line, rest);
    }
    if line.starts_with('+') || line.starts_with('-') {
        return parse_move(line);
    }
    if let Some(rest) = line.strip_prefix('#') {
        return Ok(CsaCommand::End(EndMarker::from_token(rest)));
    }
    if let Some(rest) = line.strip_prefix("LOGIN:") {
        return parse_login_response(line, rest);
    }
    if let Some(rest) = line.strip_prefix("LOGOUT:") {
        return if rest == "completed" {
            Ok(CsaCommand::LogoutCompleted)
        } else {
            Err(ParseError::new(ParseErrorKind::UnknownCommand, line))
        };
    }
    if let Some(rest) = line.strip_prefix("START:") {
        return if rest.is_empty() {
            Err(ParseError::new(ParseErrorKind::MissingArgument("game_id"), line))
        } else {
            Ok(CsaCommand::Start { game_id: rest.to_owned() })
        };
    }
    if let Some(rest) = line.strip_prefix("REJECT:") {
        return parse_reject_notice(line, rest);
    }
    if line == "BEGIN Game_Summary" {
        return Ok(CsaCommand::BeginGameSummary);
    }
    if line == "CHALLENGE ACCEPTED" {
        return Ok(CsaCommand::ChallengeAccepted);
    }
    if line == "CHALLENGE" {
        return Ok(CsaCommand::Challenge);
    }

    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("LOGIN") => parse_login(line, &mut parts),
        Some("LOGOUT") => {
            reject_trailing(line, &mut parts)?;
            Ok(CsaCommand::Logout)
        }
        Some("AGREE") => {
            let game_id = parts.next().map(str::to_owned);
            reject_trailing(line, &mut parts)?;
            Ok(CsaCommand::Agree { game_id })
        }
        Some("REJECT") => {
            let game_id = parts.next().map(str::to_owned);
            reject_trailing(line, &mut parts)?;
            Ok(CsaCommand::Reject { game_id })
        }
        _ => Err(ParseError::new(ParseErrorKind::UnknownCommand, line)),
    }
}

/// Parses one protocol line and requires it to already be canonical.
///
/// The line is parsed permissively, formatted back, and accepted only when the
/// result equals the trimmed input.
///
/// ```
/// use rsshogi_csa::parse_line_strict;
///
/// assert!(parse_line_strict("AGREE game-1").is_ok());
/// assert!(parse_line_strict("AGREE   game-1").is_err());
/// ```
pub fn parse_line_strict(raw: &str) -> Result<CsaCommand, ParseError> {
    let line = raw.trim();
    let command = parse_line(line)?;
    let canonical = format_command(&command);
    if canonical == line {
        Ok(command)
    } else {
        Err(ParseError::new(ParseErrorKind::NonCanonical { canonical }, line))
    }
}

fn parse_info(line: &str, rest: &str) -> Result<CsaCommand, ParseError> {
    let Some(end) = rest.find(']') else {
        return Err(ParseError::new(ParseErrorKind::InvalidField("info tag"), line));
    };
    let tag = rest[..end].to_owned();
    let remainder = rest[end + 1..].to_owned();
    Ok(CsaCommand::Info(InfoLine { tag, rest: remainder }))
}

fn parse_login_response(line: &str, rest: &str) -> Result<CsaCommand, ParseError> {
    if rest == "incorrect" {
        return Ok(CsaCommand::LoginIncorrect);
    }
    let Some(name) = rest.strip_suffix(" OK") else {
        return Err(ParseError::new(ParseErrorKind::UnknownCommand, line));
    };
    if name.is_empty() {
        return Err(ParseError::new(ParseErrorKind::MissingArgument("name"), line));
    }
    Ok(CsaCommand::LoginOk { name: name.to_owned() })
}

fn parse_reject_notice(line: &str, rest: &str) -> Result<CsaCommand, ParseError> {
    let Some((game_id, by)) = rest.split_once(" by ") else {
        return Err(ParseError::new(ParseErrorKind::InvalidField("reject notice"), line));
    };
    if game_id.is_empty() || by.is_empty() {
        return Err(ParseError::new(ParseErrorKind::InvalidField("reject notice"), line));
    }
    Ok(CsaCommand::RejectNotice { game_id: game_id.to_owned(), by: by.to_owned() })
}

fn parse_login<'a>(
    line: &str,
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<CsaCommand, ParseError> {
    let name = parts
        .next()
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingArgument("name"), line))?;
    let password = parts
        .next()
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingArgument("password"), line))?;
    let x1 = match parts.next() {
        None => false,
        Some("x1") => true,
        Some(_) => return Err(ParseError::new(ParseErrorKind::UnexpectedArgument, line)),
    };
    reject_trailing(line, parts)?;
    Ok(CsaCommand::Login { name: name.to_owned(), password: password.to_owned(), x1 })
}

fn parse_special(line: &str, rest: &str) -> Result<CsaCommand, ParseError> {
    let (token, elapsed) = split_time_annotation(line, rest)?;
    if token.is_empty() {
        return Err(ParseError::new(ParseErrorKind::MissingArgument("special move"), line));
    }
    Ok(CsaCommand::Special { kind: SpecialMove::from_token(token), elapsed })
}

fn parse_move(line: &str) -> Result<CsaCommand, ParseError> {
    let (head, mut cursor) = split_first_field(line);
    let Some(csa) = CsaMove::parse(head) else {
        return Err(ParseError::at(ParseErrorKind::InvalidMove, line, 0, head.len()));
    };

    let mut elapsed = None;
    let mut comment = None;
    while let Some(rest) = cursor {
        if let Some(text) = rest.strip_prefix('\'') {
            comment = Some(text.to_owned());
            break;
        }
        let (field, next) = split_first_field(rest);
        let Some(digits) = field.strip_prefix('T') else {
            return Err(ParseError::new(ParseErrorKind::InvalidField("move annotation"), line));
        };
        elapsed = Some(parse_elapsed(line, digits)?);
        cursor = next;
    }

    Ok(CsaCommand::Move(MoveLine { csa, elapsed, comment }))
}

fn parse_meta(line: &str, rest: &str) -> Result<CsaCommand, ParseError> {
    let mut parts = rest.split_whitespace();
    let Some(name) = parts.next() else {
        return Err(ParseError::new(ParseErrorKind::UnknownCommand, line));
    };

    let meta = match name {
        "GAME" | "CHALLENGE" => return parse_meta_game(line, name, &mut parts),
        "WHO" => MetaCommand::Who,
        "LIST" => MetaCommand::List,
        "RATING" => MetaCommand::Rating,
        "VERSION" => MetaCommand::Version,
        "HELP" => MetaCommand::Help,
        "SHOW" => {
            let game_id = parts
                .next()
                .ok_or_else(|| ParseError::new(ParseErrorKind::MissingArgument("game_id"), line))?;
            MetaCommand::Show(game_id.to_owned())
        }
        "CHAT" => {
            let message = rest.trim_start().strip_prefix("CHAT").unwrap_or_default().trim_start();
            if message.is_empty() {
                return Err(ParseError::new(ParseErrorKind::MissingArgument("message"), line));
            }
            return Ok(CsaCommand::Meta(MetaCommand::Chat(message.to_owned())));
        }
        other => {
            return Ok(CsaCommand::Meta(MetaCommand::Other {
                name: other.to_owned(),
                args: parts.map(str::to_owned).collect(),
            }))
        }
    };

    reject_trailing(line, &mut parts)?;
    Ok(CsaCommand::Meta(meta))
}

fn parse_meta_game<'a>(
    line: &str,
    name: &str,
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<CsaCommand, ParseError> {
    let game_name = parts.next().map(str::to_owned);
    let turn = match parts.next() {
        None => None,
        Some(token) => Some(parse_turn(line, token)?),
    };
    reject_trailing(line, parts)?;

    let meta = if name == "GAME" {
        MetaCommand::Game { name: game_name, turn }
    } else {
        let Some(game_name) = game_name else {
            return Err(ParseError::new(ParseErrorKind::MissingArgument("game name"), line));
        };
        let Some(turn) = turn else {
            return Err(ParseError::new(ParseErrorKind::MissingArgument("turn"), line));
        };
        MetaCommand::Challenge { name: game_name, turn }
    };
    Ok(CsaCommand::Meta(meta))
}

fn parse_turn(line: &str, token: &str) -> Result<TurnPreference, ParseError> {
    let mut chars = token.chars();
    let (Some(first), None) = (chars.next(), chars.next()) else {
        return Err(ParseError::new(ParseErrorKind::InvalidField("turn"), line));
    };
    TurnPreference::from_csa(first)
        .ok_or_else(|| ParseError::new(ParseErrorKind::InvalidField("turn"), line))
}

fn split_first_field(value: &str) -> (&str, Option<&str>) {
    value.split_once(',').map_or((value, None), |(head, rest)| (head, Some(rest)))
}

fn split_time_annotation<'a>(
    line: &str,
    value: &'a str,
) -> Result<(&'a str, Option<u64>), ParseError> {
    let (head, rest) = split_first_field(value);
    let Some(rest) = rest else {
        return Ok((head, None));
    };
    let Some(digits) = rest.strip_prefix('T') else {
        return Err(ParseError::new(ParseErrorKind::InvalidField("time annotation"), line));
    };
    Ok((head, Some(parse_elapsed(line, digits)?)))
}

fn parse_elapsed(line: &str, digits: &str) -> Result<u64, ParseError> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseError::new(ParseErrorKind::InvalidNumber("T"), line));
    }
    digits.parse().map_err(|_| ParseError::new(ParseErrorKind::InvalidNumber("T"), line))
}

fn reject_trailing<'a>(
    line: &str,
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<(), ParseError> {
    if parts.next().is_some() {
        Err(ParseError::new(ParseErrorKind::UnexpectedArgument, line))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CsaCommandDirection, EndMarkerKind};

    fn parse(line: &str) -> CsaCommand {
        parse_line(line).unwrap_or_else(|err| panic!("`{line}` should parse: {err}"))
    }

    #[test]
    fn blank_line_is_keepalive() {
        assert_eq!(parse(""), CsaCommand::KeepAlive);
        assert_eq!(parse("   "), CsaCommand::KeepAlive);
        assert_eq!(parse("\r\n"), CsaCommand::KeepAlive);
    }

    #[test]
    fn login_variants() {
        assert_eq!(
            parse("LOGIN engine floodgate-600-10,secret"),
            CsaCommand::Login {
                name: "engine".to_owned(),
                password: "floodgate-600-10,secret".to_owned(),
                x1: false,
            }
        );
        assert_eq!(
            parse("LOGIN engine secret x1"),
            CsaCommand::Login {
                name: "engine".to_owned(),
                password: "secret".to_owned(),
                x1: true,
            }
        );
        assert!(parse_line("LOGIN engine secret x2").is_err());
        assert!(parse_line("LOGIN engine").is_err());
    }

    #[test]
    fn login_responses() {
        assert_eq!(parse("LOGIN:engine OK"), CsaCommand::LoginOk { name: "engine".to_owned() });
        assert_eq!(parse("LOGIN:incorrect"), CsaCommand::LoginIncorrect);
        assert!(parse_line("LOGIN: OK").is_err());
        assert!(parse_line("LOGIN:engine NG").is_err());
    }

    #[test]
    fn reject_notice_needs_a_rejector() {
        assert_eq!(
            parse("REJECT:wdoor+floodgate-900-0+a+b+20260804120000 by the Server (timed out)"),
            CsaCommand::RejectNotice {
                game_id: "wdoor+floodgate-900-0+a+b+20260804120000".to_owned(),
                by: "the Server (timed out)".to_owned(),
            }
        );
        assert!(parse_line("REJECT:game-1").is_err());
    }

    #[test]
    fn client_reject_is_distinct_from_server_notice() {
        assert_eq!(parse("REJECT"), CsaCommand::Reject { game_id: None });
        assert_eq!(
            parse("REJECT game-1"),
            CsaCommand::Reject { game_id: Some("game-1".to_owned()) }
        );
        assert_eq!(parse("REJECT").direction(), Some(CsaCommandDirection::ClientToServer));
        assert_eq!(
            parse("REJECT:game-1 by foo").direction(),
            Some(CsaCommandDirection::ServerToClient)
        );
    }

    #[test]
    fn move_annotations() {
        let CsaCommand::Move(bare) = parse("+7776FU") else { panic!("expected a move") };
        assert_eq!(bare.csa.as_str(), "+7776FU");
        assert_eq!(bare.elapsed, None);
        assert_eq!(bare.comment, None);

        let CsaCommand::Move(timed) = parse("+7776FU,T12") else { panic!("expected a move") };
        assert_eq!(timed.elapsed, Some(12));

        let CsaCommand::Move(commented) = parse("-8384FU,'* 60 +2625FU -8485FU") else {
            panic!("expected a move")
        };
        assert_eq!(commented.comment.as_deref(), Some("* 60 +2625FU -8485FU"));
        assert_eq!(commented.elapsed, None);

        let CsaCommand::Move(both) = parse("-8384FU,T7,'* 60 +2625FU") else {
            panic!("expected a move")
        };
        assert_eq!(both.elapsed, Some(7));
        assert_eq!(both.comment.as_deref(), Some("* 60 +2625FU"));
    }

    #[test]
    fn move_rejects_bad_annotations() {
        assert!(parse_line("+7776FU,").is_err());
        assert!(parse_line("+7776FU,X12").is_err());
        assert!(parse_line("+7776FU,T").is_err());
        assert!(parse_line("+7776FU,Tx").is_err());
        assert!(parse_line("+7776XX,T1").is_err());
    }

    #[test]
    fn special_moves_carry_optional_time() {
        assert_eq!(
            parse("%TORYO"),
            CsaCommand::Special { kind: SpecialMove::Toryo, elapsed: None }
        );
        assert_eq!(
            parse("%KACHI,T20"),
            CsaCommand::Special { kind: SpecialMove::Kachi, elapsed: Some(20) }
        );
        assert_eq!(
            parse("%+ILLEGAL_ACTION"),
            CsaCommand::Special { kind: SpecialMove::IllegalActionBlack, elapsed: None }
        );
        let CsaCommand::Special { kind, .. } = parse("%WHATEVER") else {
            panic!("expected a special move")
        };
        assert_eq!(kind, SpecialMove::Other("WHATEVER".to_owned()));
    }

    #[test]
    fn end_markers_are_permissive() {
        assert_eq!(parse("#WIN"), CsaCommand::End(EndMarker::Win));
        assert_eq!(parse("#OUTE_SENNICHITE"), CsaCommand::End(EndMarker::OuteSennichite));
        let CsaCommand::End(marker) = parse("#FUTURE_REASON") else { panic!("expected an end") };
        assert_eq!(marker.kind(), EndMarkerKind::Unknown);
    }

    #[test]
    fn info_lines_keep_their_remainder_verbatim() {
        let CsaCommand::Info(info) = parse("##[ERROR] unknown command: foo") else {
            panic!("expected info")
        };
        assert_eq!(info.tag, "ERROR");
        assert_eq!(info.rest, " unknown command: foo");
        assert_eq!(info.body(), "unknown command: foo");

        let CsaCommand::Info(fork) = parse("##[FORK]: new buoy game name: buoy_x-900-0") else {
            panic!("expected info")
        };
        assert_eq!(fork.tag, "FORK");
        assert_eq!(fork.rest, ": new buoy game name: buoy_x-900-0");
    }

    #[test]
    fn meta_commands() {
        assert_eq!(parse("%%GAME"), CsaCommand::Meta(MetaCommand::Game { name: None, turn: None }));
        assert_eq!(
            parse("%%GAME floodgate-900-0 *"),
            CsaCommand::Meta(MetaCommand::Game {
                name: Some("floodgate-900-0".to_owned()),
                turn: Some(TurnPreference::Either),
            })
        );
        assert_eq!(
            parse("%%CHALLENGE test-1500-0 +"),
            CsaCommand::Meta(MetaCommand::Challenge {
                name: "test-1500-0".to_owned(),
                turn: TurnPreference::Black,
            })
        );
        assert_eq!(parse("%%WHO"), CsaCommand::Meta(MetaCommand::Who));
        assert_eq!(
            parse("%%SHOW game-1"),
            CsaCommand::Meta(MetaCommand::Show("game-1".to_owned()))
        );
        assert_eq!(
            parse("%%CHAT hello  world"),
            CsaCommand::Meta(MetaCommand::Chat("hello  world".to_owned()))
        );
        assert!(parse_line("%%WHO extra").is_err());
        assert!(parse_line("%%CHALLENGE only-a-name").is_err());
        assert!(parse_line("%%GAME name x").is_err());
    }

    #[test]
    fn unknown_meta_commands_round_trip_through_other() {
        assert_eq!(
            parse("%%SETBUOY buoy_x-900-0 +7776FUT2 3"),
            CsaCommand::Meta(MetaCommand::Other {
                name: "SETBUOY".to_owned(),
                args: vec!["buoy_x-900-0".to_owned(), "+7776FUT2".to_owned(), "3".to_owned()],
            })
        );
        assert_eq!(
            parse("%%%81DOJO_EXTENSION"),
            CsaCommand::Meta(MetaCommand::Other {
                name: "%81DOJO_EXTENSION".to_owned(),
                args: Vec::new(),
            })
        );
    }

    #[test]
    fn summary_field_lines_are_not_top_level_commands() {
        assert!(parse_line("Name+:engine").is_err());
        assert!(parse_line("BEGIN Time").is_err());
        assert!(parse_line("P1-KY-KE-GI-KI-OU-KI-GI-KE-KY").is_err());
        assert_eq!(parse("BEGIN Game_Summary"), CsaCommand::BeginGameSummary);
    }

    #[test]
    fn strict_mode_rejects_noncanonical_spelling() {
        assert!(parse_line_strict("AGREE game-1").is_ok());
        assert!(parse_line_strict("AGREE   game-1").is_err());
        assert!(parse_line_strict("+7776FU,T12").is_ok());
    }

    #[test]
    fn strict_mode_reports_the_canonical_spelling() {
        let error =
            parse_line_strict("LOGIN  engine  secret").expect_err("should not be canonical");
        assert_eq!(
            *error.kind(),
            ParseErrorKind::NonCanonical { canonical: "LOGIN engine secret".to_owned() }
        );
    }
}
