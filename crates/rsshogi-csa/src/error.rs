use std::error::Error;
use std::fmt;

/// Where in the offending line the problem was detected.
///
/// The range is a byte range into [`ParseErrorSite::line`] and is always valid
/// for slicing, so callers can render a caret without re-validating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseErrorSite {
    line: String,
    byte_start: usize,
    byte_end: usize,
}

impl ParseErrorSite {
    fn whole(line: &str) -> Self {
        Self { line: line.to_owned(), byte_start: 0, byte_end: line.len() }
    }

    fn span(line: &str, byte_start: usize, byte_end: usize) -> Self {
        let end = byte_end.min(line.len());
        let start = byte_start.min(end);
        Self { line: line.to_owned(), byte_start: start, byte_end: end }
    }

    /// The full line that failed to parse, without its trailing newline.
    #[must_use]
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Byte offset of the first byte of the offending fragment.
    #[must_use]
    pub const fn byte_start(&self) -> usize {
        self.byte_start
    }

    /// Byte offset one past the last byte of the offending fragment.
    #[must_use]
    pub const fn byte_end(&self) -> usize {
        self.byte_end
    }

    /// The offending fragment itself.
    #[must_use]
    pub fn fragment(&self) -> &str {
        self.line.get(self.byte_start..self.byte_end).unwrap_or(&self.line)
    }
}

impl fmt::Display for ParseErrorSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.line)
    }
}

/// What kind of problem [`crate::parse_line`] ran into.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The leading token is not a CSA protocol command this crate knows.
    UnknownCommand,
    /// A required argument is absent. Carries the name of the missing argument.
    MissingArgument(&'static str),
    /// More arguments were supplied than the command accepts.
    UnexpectedArgument,
    /// A move field is not syntactically a CSA move such as `+7776FU`.
    InvalidMove,
    /// A numeric field could not be read. Carries the name of the field.
    InvalidNumber(&'static str),
    /// A structured field is malformed. Carries the name of the field.
    InvalidField(&'static str),
    /// A line appeared where the surrounding block does not allow it.
    UnexpectedLine,
    /// Strict mode only: the line parsed, but is not the canonical spelling.
    NonCanonical {
        /// The canonical spelling of the same command.
        canonical: String,
    },
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand => f.write_str("unknown command"),
            Self::MissingArgument(name) => write!(f, "missing argument: {name}"),
            Self::UnexpectedArgument => f.write_str("unexpected argument"),
            Self::InvalidMove => f.write_str("invalid csa move"),
            Self::InvalidNumber(name) => write!(f, "invalid number in field: {name}"),
            Self::InvalidField(name) => write!(f, "invalid field: {name}"),
            Self::UnexpectedLine => f.write_str("unexpected line in block"),
            Self::NonCanonical { canonical } => {
                write!(f, "line is not canonical, expected `{canonical}`")
            }
        }
    }
}

/// A CSA protocol line could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    kind: ParseErrorKind,
    site: ParseErrorSite,
}

impl ParseError {
    pub(crate) fn new(kind: ParseErrorKind, line: &str) -> Self {
        Self { kind, site: ParseErrorSite::whole(line) }
    }

    pub(crate) fn at(kind: ParseErrorKind, line: &str, byte_start: usize, byte_end: usize) -> Self {
        Self { kind, site: ParseErrorSite::span(line, byte_start, byte_end) }
    }

    /// What went wrong.
    #[must_use]
    pub const fn kind(&self) -> &ParseErrorKind {
        &self.kind
    }

    /// Where it went wrong.
    #[must_use]
    pub const fn site(&self) -> &ParseErrorSite {
        &self.site
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.site)
    }
}

impl Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_clamps_out_of_range_spans() {
        let site = ParseErrorSite::span("AGREE", 3, 99);
        assert_eq!(site.byte_start(), 3);
        assert_eq!(site.byte_end(), 5);
        assert_eq!(site.fragment(), "EE");
    }

    #[test]
    fn site_clamps_inverted_spans() {
        let site = ParseErrorSite::span("AGREE", 4, 1);
        assert_eq!(site.byte_start(), 1);
        assert_eq!(site.byte_end(), 1);
        assert_eq!(site.fragment(), "");
    }

    #[test]
    fn display_includes_kind_and_line() {
        let error = ParseError::new(ParseErrorKind::UnknownCommand, "NOPE");
        assert_eq!(error.to_string(), "unknown command: `NOPE`");
    }
}
