//! Fixture-driven round-trip tests.
//!
//! Fixture format: one case per line, either `input` (already canonical) or
//! `input => expected`. Blank lines and lines starting with `;` are ignored.
//!
//! The comment marker is `;` rather than `#` because `#` starts every CSA
//! terminal marker, so `rsshogi-usi`'s otherwise reusable transcript helper
//! would silently skip `#WIN` and friends.

use std::fs;
use std::path::{Path, PathBuf};

use rsshogi_csa::{format_command, parse_line, parse_line_strict, CsaCommand, ParseError};

#[test]
fn valid_transcripts_round_trip_to_canonical_form() {
    for path in transcripts("valid") {
        for case in cases(&path) {
            let command = case.parse_with(parse_line, &path);
            let actual = format_command(&command);
            assert_eq!(
                actual,
                case.expected,
                "{}:{} formatted mismatch for `{}`",
                path.display(),
                case.line_no,
                case.input
            );
        }
    }
}

#[test]
fn invalid_transcripts_are_rejected() {
    for path in transcripts("invalid") {
        for case in cases(&path) {
            assert!(
                parse_line(&case.input).is_err(),
                "{}:{} should not parse: `{}`",
                path.display(),
                case.line_no,
                case.input
            );
        }
    }
}

#[test]
fn strict_valid_transcripts_accept_only_canonical_lines() {
    for path in transcripts("strict-valid") {
        for case in cases(&path) {
            let command = case.parse_with(parse_line_strict, &path);
            assert_eq!(format_command(&command), case.expected);
        }
    }
}

#[test]
fn strict_invalid_transcripts_reject_noncanonical_lines() {
    for path in transcripts("strict-invalid") {
        for case in cases(&path) {
            assert!(
                parse_line_strict(&case.input).is_err(),
                "{}:{} should be rejected as non-canonical: `{}`",
                path.display(),
                case.line_no,
                case.input
            );
        }
    }
}

#[test]
fn every_transcript_directory_has_cases() {
    for kind in ["valid", "invalid", "strict-valid", "strict-invalid"] {
        let paths = transcripts(kind);
        assert!(!paths.is_empty(), "{kind} should contain at least one fixture");
        let total: usize = paths.iter().map(|path| cases(path).len()).sum();
        assert!(total > 0, "{kind} should contain at least one case");
    }
}

struct Case {
    line_no: usize,
    input: String,
    expected: String,
}

impl Case {
    fn parse_with(
        &self,
        parse: impl Fn(&str) -> Result<CsaCommand, ParseError>,
        path: &Path,
    ) -> CsaCommand {
        parse(&self.input).unwrap_or_else(|err| {
            panic!("{}:{} should parse `{}`: {err}", path.display(), self.line_no, self.input)
        })
    }
}

fn cases(path: &Path) -> Vec<Case> {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{} should be readable: {err}", path.display()));
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(';') {
                return None;
            }
            let (input, expected) = trimmed
                .split_once("=>")
                .map_or((trimmed, trimmed), |(lhs, rhs)| (lhs.trim(), rhs.trim()));
            Some(Case {
                line_no: index + 1,
                input: input.to_owned(),
                expected: expected.to_owned(),
            })
        })
        .collect()
}

fn transcripts(kind: &str) -> Vec<PathBuf> {
    let root = Path::new("tests/transcripts").join(kind);
    let mut paths = fs::read_dir(&root)
        .unwrap_or_else(|err| panic!("{} should exist: {err}", root.display()))
        .map(|entry| entry.expect("transcript entry should be readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}
