//! Command model for the CSA server protocol.
//!
//! Everything here is line-shaped and board-agnostic. Moves are kept as
//! validated text and the `BEGIN Position` body is kept verbatim, so this crate
//! never needs a board representation.

use std::fmt;

/// Which side of the board a CSA `+` / `-` marker refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// Sente, spelled `+` on the wire.
    Black,
    /// Gote, spelled `-` on the wire.
    White,
}

impl Color {
    /// The wire character for this color.
    #[must_use]
    pub const fn as_csa(self) -> char {
        match self {
            Self::Black => '+',
            Self::White => '-',
        }
    }

    /// Reads a color from its wire character.
    #[must_use]
    pub const fn from_csa(value: char) -> Option<Self> {
        match value {
            '+' => Some(Self::Black),
            '-' => Some(Self::White),
            _ => None,
        }
    }

    /// The opposite color.
    #[must_use]
    pub const fn flip(self) -> Self {
        match self {
            Self::Black => Self::White,
            Self::White => Self::Black,
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Black => "+",
            Self::White => "-",
        })
    }
}

/// The turn a client asks for in `%%GAME` / `%%CHALLENGE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnPreference {
    /// `+`
    Black,
    /// `-`
    White,
    /// `*`, meaning the server decides.
    Either,
}

impl TurnPreference {
    /// The wire character for this preference.
    #[must_use]
    pub const fn as_csa(self) -> char {
        match self {
            Self::Black => '+',
            Self::White => '-',
            Self::Either => '*',
        }
    }

    /// Reads a preference from its wire character.
    #[must_use]
    pub const fn from_csa(value: char) -> Option<Self> {
        match value {
            '+' => Some(Self::Black),
            '-' => Some(Self::White),
            '*' => Some(Self::Either),
            _ => None,
        }
    }
}

impl fmt::Display for TurnPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_csa())
    }
}

/// The fourteen CSA piece names, in the spelling used by move records.
const PIECE_NAMES: [&str; 14] =
    ["FU", "KY", "KE", "GI", "KI", "KA", "HI", "OU", "TO", "NY", "NK", "NG", "UM", "RY"];

/// A syntactically valid CSA move such as `+7776FU`.
///
/// Validation is syntactic only: sign, valid source and destination coordinates,
/// and a known piece name.
/// Whether the move is legal in a given position is not this crate's concern.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CsaMove(String);

impl CsaMove {
    /// Parses a move, returning `None` if it is not syntactically a CSA move.
    ///
    /// ```
    /// use rsshogi_csa::CsaMove;
    ///
    /// assert!(CsaMove::parse("+7776FU").is_some());
    /// assert!(CsaMove::parse("+0076FU").is_some()); // drop
    /// assert!(CsaMove::parse("+0000FU").is_none()); // destination is not a square
    /// assert!(CsaMove::parse("+7776XX").is_none()); // unknown piece
    /// ```
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 7 {
            return None;
        }
        if bytes[0] != b'+' && bytes[0] != b'-' {
            return None;
        }
        if !(bytes[1..3] == *b"00" || is_board_square(&bytes[1..3]))
            || !is_board_square(&bytes[3..5])
        {
            return None;
        }
        let piece = &value[5..7];
        if !PIECE_NAMES.contains(&piece) {
            return None;
        }
        Some(Self(value.to_owned()))
    }

    /// The move as it appears on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The side that plays this move.
    #[must_use]
    pub fn color(&self) -> Color {
        // `parse` guarantees the first byte is `+` or `-`.
        if self.0.starts_with('+') {
            Color::Black
        } else {
            Color::White
        }
    }

    /// Whether the move drops a piece from hand, spelled with a `00` origin.
    #[must_use]
    pub fn is_drop(&self) -> bool {
        &self.0[1..3] == "00"
    }

    /// The piece name written after the destination square.
    #[must_use]
    pub fn piece_name(&self) -> &str {
        &self.0[5..7]
    }
}

fn is_board_square(bytes: &[u8]) -> bool {
    matches!(bytes, [b'1'..=b'9', b'1'..=b'9'])
}

impl fmt::Display for CsaMove {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A move line, optionally annotated with consumed time and a comment.
///
/// The server annotates broadcast moves with `,T<integer>`. Clients may
/// annotate their own moves with a `,'` comment, which floodgate uses to carry
/// evaluation and principal variation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveLine {
    /// The move itself.
    pub csa: CsaMove,
    /// Raw integer time annotation, without unit conversion.
    ///
    /// Normally expressed in `Time_Unit` units. Server implementations may
    /// differ (for example, stopwatch clocks); the session consumer must define
    /// the normalization. Fractional annotations are not supported.
    pub elapsed: Option<u64>,
    /// Comment text, without the leading `'`.
    pub comment: Option<String>,
}

impl MoveLine {
    /// A bare move with no annotations.
    #[must_use]
    pub const fn new(csa: CsaMove) -> Self {
        Self { csa, elapsed: None, comment: None }
    }
}

/// A `%`-prefixed special move.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecialMove {
    /// `%TORYO`, resignation.
    Toryo,
    /// `%KACHI`, an entering-king declaration.
    Kachi,
    /// `%CHUDAN`, an abort. Note that `shogi-server` treats this as an illegal
    /// move and awards the game to the opponent.
    Chudan,
    /// `%SENNICHITE`
    Sennichite,
    /// `%HIKIWAKE`
    Hikiwake,
    /// `%JISHOGI`
    Jishogi,
    /// `%MAX_MOVES`
    MaxMoves,
    /// `%TSUMI`
    Tsumi,
    /// `%FUZUMI`
    Fuzumi,
    /// `%TIME_UP`
    TimeUp,
    /// `%ILLEGAL_MOVE`
    IllegalMove,
    /// `%+ILLEGAL_ACTION`
    IllegalActionBlack,
    /// `%-ILLEGAL_ACTION`
    IllegalActionWhite,
    /// `%ERROR`
    Error,
    /// Any other `%` token, stored without the leading `%`.
    Other(String),
}

impl SpecialMove {
    /// The token without its leading `%`.
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Self::Toryo => "TORYO",
            Self::Kachi => "KACHI",
            Self::Chudan => "CHUDAN",
            Self::Sennichite => "SENNICHITE",
            Self::Hikiwake => "HIKIWAKE",
            Self::Jishogi => "JISHOGI",
            Self::MaxMoves => "MAX_MOVES",
            Self::Tsumi => "TSUMI",
            Self::Fuzumi => "FUZUMI",
            Self::TimeUp => "TIME_UP",
            Self::IllegalMove => "ILLEGAL_MOVE",
            Self::IllegalActionBlack => "+ILLEGAL_ACTION",
            Self::IllegalActionWhite => "-ILLEGAL_ACTION",
            Self::Error => "ERROR",
            Self::Other(token) => token,
        }
    }

    /// Reads a special move from its token, without the leading `%`.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "TORYO" => Self::Toryo,
            "KACHI" => Self::Kachi,
            "CHUDAN" => Self::Chudan,
            "SENNICHITE" => Self::Sennichite,
            "HIKIWAKE" => Self::Hikiwake,
            "JISHOGI" => Self::Jishogi,
            "MAX_MOVES" => Self::MaxMoves,
            "TSUMI" => Self::Tsumi,
            "FUZUMI" => Self::Fuzumi,
            "TIME_UP" => Self::TimeUp,
            "ILLEGAL_MOVE" => Self::IllegalMove,
            "+ILLEGAL_ACTION" => Self::IllegalActionBlack,
            "-ILLEGAL_ACTION" => Self::IllegalActionWhite,
            "ERROR" => Self::Error,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// How a `#`-prefixed end marker classifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndMarkerKind {
    /// States who won: `#WIN`, `#LOSE`, `#DRAW`, `#CENSORED`.
    Outcome,
    /// States why the game ended: `#RESIGN`, `#TIME_UP`, and friends.
    Reason,
    /// An unrecognized marker.
    Unknown,
}

/// A `#`-prefixed terminal marker.
///
/// A finished game normally produces a short burst of these, for example
/// `%TORYO`, `#RESIGN`, `#WIN`. `#CHUDAN` can arrive on its own.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndMarker {
    /// `#WIN`
    Win,
    /// `#LOSE`
    Lose,
    /// `#DRAW`
    Draw,
    /// `#CENSORED`
    Censored,
    /// `#CHUDAN`
    Chudan,
    /// `#RESIGN`
    Resign,
    /// `#TIME_UP`
    TimeUp,
    /// `#ILLEGAL_MOVE`
    IllegalMove,
    /// `#ILLEGAL_ACTION`
    IllegalAction,
    /// `#JISHOGI`
    Jishogi,
    /// `#SENNICHITE`
    Sennichite,
    /// `#OUTE_SENNICHITE`
    OuteSennichite,
    /// `#MAX_MOVES`
    MaxMoves,
    /// Any other `#` token, stored without the leading `#`.
    Other(String),
}

impl EndMarker {
    /// The token without its leading `#`.
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Self::Win => "WIN",
            Self::Lose => "LOSE",
            Self::Draw => "DRAW",
            Self::Censored => "CENSORED",
            Self::Chudan => "CHUDAN",
            Self::Resign => "RESIGN",
            Self::TimeUp => "TIME_UP",
            Self::IllegalMove => "ILLEGAL_MOVE",
            Self::IllegalAction => "ILLEGAL_ACTION",
            Self::Jishogi => "JISHOGI",
            Self::Sennichite => "SENNICHITE",
            Self::OuteSennichite => "OUTE_SENNICHITE",
            Self::MaxMoves => "MAX_MOVES",
            Self::Other(token) => token,
        }
    }

    /// Reads an end marker from its token, without the leading `#`.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "WIN" => Self::Win,
            "LOSE" => Self::Lose,
            "DRAW" => Self::Draw,
            "CENSORED" => Self::Censored,
            "CHUDAN" => Self::Chudan,
            "RESIGN" => Self::Resign,
            "TIME_UP" => Self::TimeUp,
            "ILLEGAL_MOVE" => Self::IllegalMove,
            "ILLEGAL_ACTION" => Self::IllegalAction,
            "JISHOGI" => Self::Jishogi,
            "SENNICHITE" => Self::Sennichite,
            "OUTE_SENNICHITE" => Self::OuteSennichite,
            "MAX_MOVES" => Self::MaxMoves,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Whether this marker states an outcome or a reason.
    #[must_use]
    pub const fn kind(&self) -> EndMarkerKind {
        match self {
            Self::Win | Self::Lose | Self::Draw | Self::Censored => EndMarkerKind::Outcome,
            Self::Chudan
            | Self::Resign
            | Self::TimeUp
            | Self::IllegalMove
            | Self::IllegalAction
            | Self::Jishogi
            | Self::Sennichite
            | Self::OuteSennichite
            | Self::MaxMoves => EndMarkerKind::Reason,
            Self::Other(_) => EndMarkerKind::Unknown,
        }
    }
}

/// A `##[TAG] ...` informational line from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoLine {
    /// The tag between the brackets, such as `ERROR` or `WHO`.
    pub tag: String,
    /// Everything after the closing bracket, kept verbatim so the line round-trips.
    ///
    /// Most lines have a single leading space here. Use [`InfoLine::body`] to
    /// read the text without it.
    pub rest: String,
}

impl InfoLine {
    /// The informational text with leading whitespace removed.
    #[must_use]
    pub fn body(&self) -> &str {
        self.rest.trim_start()
    }
}

/// A `%%`-prefixed administrative command.
///
/// Only the commands the client flow needs are typed. Everything else round-trips
/// through [`MetaCommand::Other`] rather than growing a variant per server dialect.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaCommand {
    /// `%%GAME`, `%%GAME <name>`, or `%%GAME <name> <turn>`.
    ///
    /// The bare form cancels an outstanding game request.
    Game {
        /// The requested game name, such as `floodgate-900-0`.
        name: Option<String>,
        /// The requested turn.
        turn: Option<TurnPreference>,
    },
    /// `%%CHALLENGE <name> <turn>`.
    Challenge {
        /// The requested game name.
        name: String,
        /// The requested turn.
        turn: TurnPreference,
    },
    /// `%%WHO`
    Who,
    /// `%%LIST`
    List,
    /// `%%SHOW <game_id>`
    Show(String),
    /// `%%CHAT <message>`
    Chat(String),
    /// `%%RATING`
    Rating,
    /// `%%VERSION`
    Version,
    /// `%%HELP`
    Help,
    /// Any other `%%` command. `name` excludes the leading `%%`.
    Other {
        /// The command name without its `%%` prefix.
        name: String,
        /// Remaining whitespace-separated arguments.
        args: Vec<String>,
    },
}

/// Which way a command travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CsaCommandDirection {
    /// Only a client sends this.
    ClientToServer,
    /// Only a server sends this.
    ServerToClient,
    /// Both sides send this.
    Both,
}

/// One line of the CSA server protocol.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsaCommand {
    /// `LOGIN <name> <password>` with an optional ` x1` suffix.
    Login {
        /// The account name.
        name: String,
        /// The password field, which in CSA mode also carries the game name.
        password: String,
        /// Whether the `x1` extension was requested.
        x1: bool,
    },
    /// `LOGOUT`
    Logout,
    /// `AGREE` or `AGREE <game_id>`.
    Agree {
        /// The game id echoed back to the server.
        game_id: Option<String>,
    },
    /// `REJECT` or `REJECT <game_id>`, sent by the client.
    Reject {
        /// The game id echoed back to the server.
        game_id: Option<String>,
    },
    /// `CHALLENGE`, a legacy no-op the server answers with `CHALLENGE ACCEPTED`.
    Challenge,
    /// A `%%` administrative command.
    Meta(MetaCommand),

    /// `LOGIN:<name> OK`
    LoginOk {
        /// The account name the server accepted.
        name: String,
    },
    /// `LOGIN:incorrect`
    LoginIncorrect,
    /// `LOGOUT:completed`
    LogoutCompleted,
    /// `CHALLENGE ACCEPTED`
    ChallengeAccepted,
    /// `BEGIN Game_Summary`, the first line of a game proposal.
    ///
    /// Feed this line and everything after it to [`crate::GameSummaryReader`].
    BeginGameSummary,
    /// `START:<game_id>`
    Start {
        /// The game id that is now in progress.
        game_id: String,
    },
    /// `REJECT:<game_id> by <name>`, sent by the server to both players.
    RejectNotice {
        /// The rejected game id.
        game_id: String,
        /// Who rejected it, or `the Server (timed out)`.
        by: String,
    },
    /// A `#`-prefixed terminal marker.
    End(EndMarker),
    /// A `##[TAG] ...` informational line.
    Info(InfoLine),

    /// A move, optionally annotated with elapsed time and a comment.
    Move(MoveLine),
    /// A `%`-prefixed special move, optionally annotated with elapsed time.
    Special {
        /// Which special move.
        kind: SpecialMove,
        /// Raw integer time annotation, with the same unit contract as [`MoveLine::elapsed`].
        elapsed: Option<u64>,
    },
    /// An empty line, used as an application-level keepalive.
    ///
    /// The CSA rule is that a client must not send these more often than once
    /// every 30 seconds.
    KeepAlive,
}

impl CsaCommand {
    /// Which way this command travels, or `None` when the direction is unknown.
    #[must_use]
    pub const fn direction(&self) -> Option<CsaCommandDirection> {
        match self {
            Self::Login { .. }
            | Self::Logout
            | Self::Agree { .. }
            | Self::Reject { .. }
            | Self::Challenge => Some(CsaCommandDirection::ClientToServer),
            Self::Meta(meta) => match meta {
                MetaCommand::Other { .. } => None,
                _ => Some(CsaCommandDirection::ClientToServer),
            },
            Self::LoginOk { .. }
            | Self::LoginIncorrect
            | Self::LogoutCompleted
            | Self::ChallengeAccepted
            | Self::BeginGameSummary
            | Self::Start { .. }
            | Self::RejectNotice { .. }
            | Self::End(_)
            | Self::Info(_) => Some(CsaCommandDirection::ServerToClient),
            Self::Move(_) | Self::Special { .. } | Self::KeepAlive => {
                Some(CsaCommandDirection::Both)
            }
        }
    }

    /// Whether a client may send this command.
    #[must_use]
    pub const fn is_client_to_server(&self) -> bool {
        matches!(
            self.direction(),
            Some(CsaCommandDirection::ClientToServer | CsaCommandDirection::Both)
        )
    }

    /// Whether a server may send this command.
    #[must_use]
    pub const fn is_server_to_client(&self) -> bool {
        matches!(
            self.direction(),
            Some(CsaCommandDirection::ServerToClient | CsaCommandDirection::Both)
        )
    }
}

/// The scale part of a `Time_Unit` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeScale {
    /// `msec`
    Msec,
    /// `sec`
    Sec,
    /// `min`
    Min,
}

impl TimeScale {
    /// Milliseconds in one unit of this scale.
    #[must_use]
    pub const fn millis(self) -> u64 {
        match self {
            Self::Msec => 1,
            Self::Sec => 1_000,
            Self::Min => 60_000,
        }
    }

    /// The suffix used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Msec => "msec",
            Self::Sec => "sec",
            Self::Min => "min",
        }
    }
}

/// A `Time_Unit` value such as `1sec` or `1min`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeUnit {
    /// How many scale units make one time unit.
    pub count: u64,
    /// The scale.
    pub scale: TimeScale,
}

impl TimeUnit {
    /// One second, the value `shogi-server` always sends for chess-clock games.
    pub const SECOND: Self = Self { count: 1, scale: TimeScale::Sec };

    /// The unit expressed in milliseconds, saturating instead of overflowing.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.count.saturating_mul(self.scale.millis())
    }
}

impl Default for TimeUnit {
    fn default() -> Self {
        Self::SECOND
    }
}

impl fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.count, self.scale.as_str())
    }
}

/// The time control of one side, as declared in a `BEGIN Time` block.
///
/// All durations are in `Time_Unit` units, exactly as they appear on the wire.
/// Converting to milliseconds is the caller's job, because only the caller knows
/// whether it wants to apply a safety margin.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeConfig {
    /// `Time_Unit`
    pub time_unit: Option<TimeUnit>,
    /// `Total_Time`
    pub total_time: Option<u64>,
    /// `Byoyomi`
    pub byoyomi: Option<u64>,
    /// `Delay`
    pub delay: Option<u64>,
    /// `Increment`, granted up front by the server.
    pub increment: Option<u64>,
    /// `Least_Time_Per_Move`
    pub least_time_per_move: Option<u64>,
    /// `Time_Roundup`
    pub time_roundup: Option<bool>,
    /// `Remaining_Time+` or `Remaining_Time-`, present only in `%%SHOW` output.
    pub remaining_time: Option<u64>,
    /// Unrecognized `Key:Value` pairs, kept in wire order.
    pub extras: Vec<(String, String)>,
}

impl TimeConfig {
    /// A control that declares only its time unit.
    ///
    /// The type is `#[non_exhaustive]` because CSA can add time fields, so it is
    /// built with these rather than with a struct literal.
    ///
    /// ```
    /// use rsshogi_csa::{TimeConfig, TimeUnit};
    ///
    /// let control = TimeConfig::new(TimeUnit::SECOND).with_total_time(900).with_byoyomi(10);
    /// assert_eq!(control.total_time, Some(900));
    /// assert_eq!(control.byoyomi, Some(10));
    /// ```
    #[must_use]
    pub fn new(unit: TimeUnit) -> Self {
        Self { time_unit: Some(unit), ..Self::default() }
    }

    /// Sets `Total_Time`.
    #[must_use]
    pub const fn with_total_time(mut self, units: u64) -> Self {
        self.total_time = Some(units);
        self
    }

    /// Sets `Byoyomi`.
    #[must_use]
    pub const fn with_byoyomi(mut self, units: u64) -> Self {
        self.byoyomi = Some(units);
        self
    }

    /// Sets `Increment`.
    #[must_use]
    pub const fn with_increment(mut self, units: u64) -> Self {
        self.increment = Some(units);
        self
    }

    /// Sets `Delay`.
    #[must_use]
    pub const fn with_delay(mut self, units: u64) -> Self {
        self.delay = Some(units);
        self
    }

    /// Sets `Least_Time_Per_Move`.
    #[must_use]
    pub const fn with_least_time_per_move(mut self, units: u64) -> Self {
        self.least_time_per_move = Some(units);
        self
    }

    /// Sets `Time_Roundup`.
    #[must_use]
    pub const fn with_time_roundup(mut self, roundup: bool) -> Self {
        self.time_roundup = Some(roundup);
        self
    }

    /// Sets `Remaining_Time`.
    #[must_use]
    pub const fn with_remaining_time(mut self, units: u64) -> Self {
        self.remaining_time = Some(units);
        self
    }

    /// Whether two sides declare the same control, ignoring remaining time.
    ///
    /// `shogi-server` sends one shared `BEGIN Time` block, so this is what
    /// decides whether [`GameSummary::to_lines`] emits a shared or split block.
    #[must_use]
    pub fn eq_ignoring_remaining(&self, other: &Self) -> bool {
        self.time_unit == other.time_unit
            && self.total_time == other.total_time
            && self.byoyomi == other.byoyomi
            && self.delay == other.delay
            && self.increment == other.increment
            && self.least_time_per_move == other.least_time_per_move
            && self.time_roundup == other.time_roundup
            && self.extras == other.extras
    }

    /// The declared time unit, defaulting to one second when absent.
    #[must_use]
    pub fn unit(&self) -> TimeUnit {
        self.time_unit.unwrap_or_default()
    }
}

/// A parsed `BEGIN Game_Summary` block.
///
/// Field order in [`GameSummary::to_lines`] follows `shogi-server`. A block that
/// declared identical per-color `BEGIN Time+` / `BEGIN Time-` sections is
/// re-emitted as a single shared `BEGIN Time` section, so block round-trips are
/// semantic rather than byte-exact.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameSummary {
    /// `Protocol_Version`
    pub protocol_version: Option<String>,
    /// `Protocol_Mode`, `Server` for every server this crate targets.
    pub protocol_mode: Option<String>,
    /// `Format`
    pub format: Option<String>,
    /// `Declaration`, such as `Jishogi 1.1`.
    pub declaration: Option<String>,
    /// `Game_ID`
    pub game_id: Option<String>,
    /// `Name+`
    pub name_black: Option<String>,
    /// `Name-`
    pub name_white: Option<String>,
    /// `Your_Turn`, absent in `%%SHOW` output.
    pub your_turn: Option<Color>,
    /// `To_Move`
    pub to_move: Option<Color>,
    /// `Rematch_On_Draw`
    pub rematch_on_draw: Option<String>,
    /// `Max_Moves`
    pub max_moves: Option<u32>,
    /// Time control for black.
    pub time_black: TimeConfig,
    /// Time control for white.
    pub time_white: TimeConfig,
    /// `Last_Move`, present only in `%%SHOW` output.
    pub last_move: Option<String>,
    /// `Current_Turn`, present only in `%%SHOW` output.
    pub current_turn: Option<u32>,
    /// The `BEGIN Position` body, verbatim and without trailing newlines.
    pub position_lines: Vec<String>,
    /// Unrecognized top-level `Key:Value` pairs, kept in wire order.
    pub extras: Vec<(String, String)>,
}

impl GameSummary {
    /// The time control for one side.
    #[must_use]
    pub const fn time(&self, color: Color) -> &TimeConfig {
        match color {
            Color::Black => &self.time_black,
            Color::White => &self.time_white,
        }
    }

    /// Splits the position body into the initial position and any replayed moves.
    ///
    /// A resumed or buoy game carries moves inside `BEGIN Position`. The split
    /// point is the first line that looks like a move, which is purely syntactic:
    /// a sign followed by four digits. Board rows start with `P` and the side-to-move
    /// line is a bare `+` or `-`, so neither can be mistaken for a move.
    #[must_use]
    pub fn position_split(&self) -> (&[String], &[String]) {
        let index = self
            .position_lines
            .iter()
            .position(|line| Self::looks_like_move_line(line))
            .unwrap_or(self.position_lines.len());
        self.position_lines.split_at(index)
    }

    fn looks_like_move_line(line: &str) -> bool {
        let bytes = line.as_bytes();
        bytes.len() >= 7
            && (bytes[0] == b'+' || bytes[0] == b'-')
            && bytes[1..5].iter().all(u8::is_ascii_digit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_parse_rejects_bad_shapes() {
        assert!(CsaMove::parse("+7776FU").is_some());
        assert!(CsaMove::parse("-3334FU").is_some());
        assert!(CsaMove::parse("+0055KA").is_some());
        assert!(CsaMove::parse("+0000FU").is_none());
        assert!(CsaMove::parse("+1099FU").is_none());
        assert!(CsaMove::parse("+9910FU").is_none());
        assert!(CsaMove::parse("7776FU").is_none());
        assert!(CsaMove::parse("+7776F").is_none());
        assert!(CsaMove::parse("+7776FUU").is_none());
        assert!(CsaMove::parse("+77a6FU").is_none());
        assert!(CsaMove::parse("+7776fu").is_none());
        assert!(CsaMove::parse("+7776XX").is_none());
    }

    #[test]
    fn move_accessors_read_the_wire_text() {
        let mv = CsaMove::parse("-0084FU").expect("valid drop");
        assert_eq!(mv.color(), Color::White);
        assert!(mv.is_drop());
        assert_eq!(mv.piece_name(), "FU");
        assert_eq!(mv.as_str(), "-0084FU");

        let mv = CsaMove::parse("+7776FU").expect("valid move");
        assert_eq!(mv.color(), Color::Black);
        assert!(!mv.is_drop());
    }

    #[test]
    fn end_marker_classification() {
        assert_eq!(EndMarker::Win.kind(), EndMarkerKind::Outcome);
        assert_eq!(EndMarker::Censored.kind(), EndMarkerKind::Outcome);
        assert_eq!(EndMarker::Resign.kind(), EndMarkerKind::Reason);
        assert_eq!(EndMarker::Chudan.kind(), EndMarkerKind::Reason);
        assert_eq!(EndMarker::from_token("NOPE").kind(), EndMarkerKind::Unknown);
    }

    #[test]
    fn time_unit_converts_to_millis() {
        assert_eq!(TimeUnit::SECOND.as_millis(), 1_000);
        assert_eq!(TimeUnit { count: 1, scale: TimeScale::Min }.as_millis(), 60_000);
        assert_eq!(TimeUnit { count: 250, scale: TimeScale::Msec }.as_millis(), 250);
        assert_eq!(TimeUnit { count: u64::MAX, scale: TimeScale::Min }.as_millis(), u64::MAX);
    }

    #[test]
    fn position_split_finds_replayed_moves() {
        let summary = GameSummary {
            position_lines: vec![
                "P1-KY-KE-GI-KI-OU-KI-GI-KE-KY".to_owned(),
                "P+".to_owned(),
                "+".to_owned(),
                "+7776FU,T2".to_owned(),
                "-3334FU,T5".to_owned(),
            ],
            ..GameSummary::default()
        };
        let (board, moves) = summary.position_split();
        assert_eq!(board.len(), 3);
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0], "+7776FU,T2");
    }

    #[test]
    fn position_split_without_moves_returns_everything_as_board() {
        let summary = GameSummary {
            position_lines: vec!["PI".to_owned(), "+".to_owned()],
            ..GameSummary::default()
        };
        let (board, moves) = summary.position_split();
        assert_eq!(board.len(), 2);
        assert!(moves.is_empty());
    }

    #[test]
    fn direction_is_derived_not_stored() {
        assert_eq!(CsaCommand::Logout.direction(), Some(CsaCommandDirection::ClientToServer));
        assert_eq!(
            CsaCommand::LoginIncorrect.direction(),
            Some(CsaCommandDirection::ServerToClient)
        );
        assert_eq!(CsaCommand::KeepAlive.direction(), Some(CsaCommandDirection::Both));
        assert_eq!(
            CsaCommand::Meta(MetaCommand::Other { name: "X".to_owned(), args: Vec::new() })
                .direction(),
            None
        );
        assert!(CsaCommand::KeepAlive.is_client_to_server());
        assert!(CsaCommand::KeepAlive.is_server_to_client());
    }
}
