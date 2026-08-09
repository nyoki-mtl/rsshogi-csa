//! The round trip in the direction the fixtures do not cover.
//!
//! `tests/transcripts.rs` starts from a wire line and checks
//! `format(parse(line)) == line`. That proves the parser and the formatter agree
//! about lines a server actually sends.
//!
//! This file starts from a *constructed* command and checks
//! `parse(format(command)) == command`. That is what catches a command a caller
//! can build but the parser cannot read back — the failure mode where the bridge
//! sends something and then cannot make sense of the echo.
//!
//! The property holds for well-formed values only. A command carrying an empty
//! chat message or an empty special-move token formats to something the parser
//! rightly rejects, and no caller has a reason to build one.

use rsshogi_csa::{
    format_command, parse_line, CsaCommand, CsaMove, EndMarker, InfoLine, MetaCommand, MoveLine,
    SpecialMove, TurnPreference,
};

fn assert_roundtrip(command: &CsaCommand) {
    let line = format_command(command);
    let parsed = parse_line(&line)
        .unwrap_or_else(|err| panic!("`{line}` was formatted but does not parse: {err}"));
    assert_eq!(&parsed, command, "`{line}` did not survive the round trip");
}

fn mv(csa: &str) -> CsaMove {
    CsaMove::parse(csa).unwrap_or_else(|| panic!("`{csa}` should be a move"))
}

#[test]
fn client_commands_survive_a_construct_format_parse_cycle() {
    for command in [
        CsaCommand::Login {
            name: "engine".to_owned(),
            password: "floodgate-900-0,secret".to_owned(),
            x1: false,
        },
        CsaCommand::Login { name: "engine".to_owned(), password: "secret".to_owned(), x1: true },
        CsaCommand::Logout,
        CsaCommand::Agree { game_id: None },
        CsaCommand::Agree { game_id: Some("game-1".to_owned()) },
        CsaCommand::Reject { game_id: None },
        CsaCommand::Reject { game_id: Some("game-1".to_owned()) },
        CsaCommand::Challenge,
        CsaCommand::KeepAlive,
    ] {
        assert_roundtrip(&command);
    }
}

#[test]
fn server_commands_survive_a_construct_format_parse_cycle() {
    for command in [
        CsaCommand::LoginOk { name: "engine".to_owned() },
        CsaCommand::LoginIncorrect,
        CsaCommand::LogoutCompleted,
        CsaCommand::ChallengeAccepted,
        CsaCommand::BeginGameSummary,
        CsaCommand::Start { game_id: "wdoor+floodgate-900-0+a+b+20260804120000".to_owned() },
        CsaCommand::RejectNotice {
            game_id: "game-1".to_owned(),
            by: "the Server (timed out)".to_owned(),
        },
        CsaCommand::Info(InfoLine { tag: "ERROR".to_owned(), rest: " nope".to_owned() }),
        CsaCommand::Info(InfoLine { tag: "LIST".to_owned(), rest: " +OK".to_owned() }),
        CsaCommand::Info(InfoLine { tag: "FORK".to_owned(), rest: ": buoy_x-900-0".to_owned() }),
    ] {
        assert_roundtrip(&command);
    }
}

#[test]
fn every_end_marker_survives_a_round_trip() {
    for marker in [
        EndMarker::Win,
        EndMarker::Lose,
        EndMarker::Draw,
        EndMarker::Censored,
        EndMarker::Chudan,
        EndMarker::Resign,
        EndMarker::TimeUp,
        EndMarker::IllegalMove,
        EndMarker::IllegalAction,
        EndMarker::Jishogi,
        EndMarker::Sennichite,
        EndMarker::OuteSennichite,
        EndMarker::MaxMoves,
        EndMarker::Other("SOMETHING_NEW".to_owned()),
    ] {
        assert_roundtrip(&CsaCommand::End(marker));
    }
}

#[test]
fn every_special_move_survives_a_round_trip() {
    for kind in [
        SpecialMove::Toryo,
        SpecialMove::Kachi,
        SpecialMove::Chudan,
        SpecialMove::Sennichite,
        SpecialMove::Hikiwake,
        SpecialMove::Jishogi,
        SpecialMove::MaxMoves,
        SpecialMove::Tsumi,
        SpecialMove::Fuzumi,
        SpecialMove::TimeUp,
        SpecialMove::IllegalMove,
        SpecialMove::IllegalActionBlack,
        SpecialMove::IllegalActionWhite,
        SpecialMove::Error,
        SpecialMove::Other("SOMETHING_NEW".to_owned()),
    ] {
        assert_roundtrip(&CsaCommand::Special { kind: kind.clone(), elapsed: None });
        assert_roundtrip(&CsaCommand::Special { kind, elapsed: Some(20) });
    }
}

#[test]
fn every_meta_command_survives_a_round_trip() {
    for meta in [
        MetaCommand::Game { name: None, turn: None },
        MetaCommand::Game { name: Some("test-1500-0".to_owned()), turn: None },
        MetaCommand::Game {
            name: Some("floodgate-900-0".to_owned()),
            turn: Some(TurnPreference::Either),
        },
        MetaCommand::Challenge { name: "test-1500-0".to_owned(), turn: TurnPreference::Black },
        MetaCommand::Who,
        MetaCommand::List,
        MetaCommand::Show("game-1".to_owned()),
        MetaCommand::Chat("hello world".to_owned()),
        MetaCommand::Rating,
        MetaCommand::Version,
        MetaCommand::Help,
        MetaCommand::Other {
            name: "SETBUOY".to_owned(),
            args: vec!["buoy_x-900-0".to_owned(), "+7776FUT2".to_owned()],
        },
        MetaCommand::Other { name: "NOARGS".to_owned(), args: Vec::new() },
    ] {
        assert_roundtrip(&CsaCommand::Meta(meta));
    }
}

#[test]
fn every_turn_preference_survives_a_round_trip() {
    for turn in [TurnPreference::Black, TurnPreference::White, TurnPreference::Either] {
        assert_roundtrip(&CsaCommand::Meta(MetaCommand::Game {
            name: Some("test-1500-0".to_owned()),
            turn: Some(turn),
        }));
    }
}

#[test]
fn move_annotations_survive_every_combination() {
    for csa in ["+7776FU", "-3334FU", "+0055KA", "-0084FU", "+8822UM", "-2233RY"] {
        for elapsed in [None, Some(0), Some(1), Some(1_234)] {
            for comment in
                [None, Some("* 60".to_owned()), Some("* -123 +2625FU -8485FU".to_owned())]
            {
                assert_roundtrip(&CsaCommand::Move(MoveLine {
                    csa: mv(csa),
                    elapsed,
                    comment: comment.clone(),
                }));
            }
        }
    }
}

#[test]
fn every_piece_name_is_accepted_and_survives() {
    for piece in
        ["FU", "KY", "KE", "GI", "KI", "KA", "HI", "OU", "TO", "NY", "NK", "NG", "UM", "RY"]
    {
        for sign in ['+', '-'] {
            let csa = format!("{sign}7776{piece}");
            let parsed = CsaMove::parse(&csa).unwrap_or_else(|| panic!("`{csa}` should parse"));
            assert_eq!(parsed.piece_name(), piece);
            assert_roundtrip(&CsaCommand::Move(MoveLine::new(parsed)));
        }
    }
}

#[test]
fn adversarial_input_never_panics() {
    // A parser fed by a network peer must not be able to abort the process. The
    // interesting cases are multi-byte characters landing inside the fixed-width
    // slices the move and info parsers use.
    for line in [
        "",
        " ",
        "\r",
        "\u{0}",
        "+",
        "-",
        "#",
        "%",
        "%%",
        "##",
        "##[",
        "##[]",
        "+77あFU",
        "+7776あ",
        "+7776FU,Tあ",
        "+7776FU,'",
        "+7776FU,",
        "+7776FU,,",
        "-000000",
        "LOGIN",
        "LOGIN ",
        "LOGIN:",
        "START:",
        "REJECT:",
        "REJECT: by ",
        "%%CHAT",
        "%%GAME あ い",
        "あいうえお",
        "\u{1F600}",
        "+\u{1F600}FU",
        "##[\u{1F600}] body",
    ] {
        // Both entry points, and the result is irrelevant. Not panicking is the test.
        let _ = parse_line(line);
        let _ = rsshogi_csa::parse_line_strict(line);
    }
}

#[test]
fn a_long_line_is_handled_without_panicking() {
    let long_comment = "x".repeat(100_000);
    let line = format!("+7776FU,'{long_comment}");
    let parsed = parse_line(&line).expect("a long comment is still a comment");
    assert_eq!(format_command(&parsed), line);

    let long_garbage = "A".repeat(100_000);
    let _ = parse_line(&long_garbage);
}
