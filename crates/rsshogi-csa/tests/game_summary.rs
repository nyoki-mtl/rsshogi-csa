//! Golden `BEGIN Game_Summary` blocks captured from `shogi-server` output.
//!
//! Each fixture must survive a read/write round trip byte for byte, which is
//! what pins the canonical field order.

use std::fs;
use std::path::{Path, PathBuf};

use rsshogi_csa::{parse_line, Color, CsaCommand, GameSummary, GameSummaryReader, TimeUnit};

#[test]
fn golden_summaries_round_trip_line_for_line() {
    let paths = fixtures();
    assert!(!paths.is_empty(), "tests/summaries should contain fixtures");

    for path in paths {
        let content = read(&path);
        let summary = parse_block(&content, &path);
        let expected = content.lines().collect::<Vec<_>>();
        assert_eq!(summary.to_lines(), expected, "{} should round trip", path.display());
    }
}

#[test]
fn floodgate_proposal_is_typed_and_keeps_the_board_verbatim() {
    let content = read(&fixture("floodgate-proposal.txt"));
    let summary = parse_block(&content, Path::new("floodgate-proposal.txt"));

    assert_eq!(summary.protocol_version.as_deref(), Some("1.2"));
    assert_eq!(summary.protocol_mode.as_deref(), Some("Server"));
    assert_eq!(summary.declaration.as_deref(), Some("Jishogi 1.1"));
    assert_eq!(summary.your_turn, Some(Color::Black));
    assert_eq!(summary.to_move, Some(Color::Black));
    assert_eq!(summary.max_moves, Some(256));
    assert_eq!(summary.name_black.as_deref(), Some("black_engine"));
    assert_eq!(summary.name_white.as_deref(), Some("white_engine"));

    assert_eq!(summary.time(Color::Black).unit(), TimeUnit::SECOND);
    assert_eq!(summary.time(Color::Black).total_time, Some(900));
    assert_eq!(summary.time(Color::White).total_time, Some(900));
    assert_eq!(summary.time(Color::Black).byoyomi, Some(0));
    assert_eq!(summary.time(Color::Black).increment, Some(0));

    // Board rows carry significant trailing spaces; losing them would corrupt
    // the position when it is handed to a record parser.
    assert_eq!(summary.position_lines.len(), 10);
    assert!(summary.position_lines[1].ends_with("-KA * "));

    let (board, moves) = summary.position_split();
    assert_eq!(board.len(), 10);
    assert!(moves.is_empty());
}

#[test]
fn buoy_proposal_exposes_replayed_moves() {
    let content = read(&fixture("buoy-proposal.txt"));
    let summary = parse_block(&content, Path::new("buoy-proposal.txt"));

    assert_eq!(summary.your_turn, Some(Color::White));
    assert_eq!(summary.time(Color::Black).byoyomi, Some(60));

    let (board, moves) = summary.position_split();
    assert_eq!(board, ["PI".to_owned(), "+".to_owned()]);
    assert_eq!(moves.len(), 3);

    // The move lines are ordinary protocol lines, so the line parser reads them.
    let mut elapsed = Vec::new();
    for line in moves {
        let CsaCommand::Move(parsed) = parse_line(line).expect("replayed move should parse") else {
            panic!("expected a move line");
        };
        elapsed.push(parsed.elapsed.expect("replayed moves carry their time"));
    }
    assert_eq!(elapsed, [2, 5, 1]);
}

#[test]
fn show_output_exposes_remaining_time_per_side() {
    let content = read(&fixture("show-output.txt"));
    let summary = parse_block(&content, Path::new("show-output.txt"));

    assert_eq!(summary.your_turn, None);
    assert_eq!(summary.time(Color::Black).remaining_time, Some(874));
    assert_eq!(summary.time(Color::White).remaining_time, Some(889));
    assert_eq!(summary.last_move.as_deref(), Some("+7776FU,T12"));
    assert_eq!(summary.current_turn, Some(3));
}

fn parse_block(content: &str, path: &Path) -> GameSummary {
    let mut reader = GameSummaryReader::new();
    let mut summary = None;
    for (index, line) in content.lines().enumerate() {
        let done = reader.feed(line).unwrap_or_else(|err| {
            panic!("{}:{} should be accepted: {err}", path.display(), index + 1)
        });
        if let Some(done) = done {
            summary = Some(done);
        }
    }
    assert!(reader.is_complete(), "{} should close the block", path.display());
    summary.unwrap_or_else(|| panic!("{} should produce a summary", path.display()))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{} should be readable: {err}", path.display()))
}

fn fixture(name: &str) -> PathBuf {
    Path::new("tests/summaries").join(name)
}

fn fixtures() -> Vec<PathBuf> {
    let root = Path::new("tests/summaries");
    let mut paths = fs::read_dir(root)
        .unwrap_or_else(|err| panic!("{} should exist: {err}", root.display()))
        .map(|entry| entry.expect("fixture entry should be readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}
