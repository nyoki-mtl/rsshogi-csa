//! Engine-agnostic CSA server protocol surface.
//!
//! This crate turns one line of the CSA server protocol into a typed
//! [`CsaCommand`] and back. It has no dependencies, no I/O, and no board
//! representation: sockets, clocks, and legality belong to the layer above.
//!
//! ```
//! use rsshogi_csa::{format_command, parse_line, CsaCommand, EndMarker};
//!
//! let command = parse_line("+7776FU,T12")?;
//! assert_eq!(format_command(&command), "+7776FU,T12");
//!
//! let CsaCommand::Move(line) = command else { unreachable!() };
//! assert_eq!(line.csa.as_str(), "+7776FU");
//! assert_eq!(line.elapsed, Some(12));
//!
//! assert_eq!(parse_line("#RESIGN")?, CsaCommand::End(EndMarker::Resign));
//! # Ok::<(), rsshogi_csa::ParseError>(())
//! ```
//!
//! The `BEGIN Game_Summary` proposal spans many lines and is assembled by
//! [`GameSummaryReader`] instead, which keeps the `BEGIN Position` body verbatim.
//!
//! # Scope
//!
//! CSA has two things that share a name. The *record format* — `.csa` files with
//! a `V2.2` header and `$EVENT` metadata — is handled by the `records` feature of
//! the [`rsshogi`](https://crates.io/crates/rsshogi) crate. The *server protocol*
//! — `LOGIN`, `BEGIN Game_Summary`, `+7776FU,T12`, `#WIN` — is what this crate
//! handles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;
mod format;
mod model;
mod parser;
mod summary;

pub use error::{ParseError, ParseErrorKind, ParseErrorSite};
pub use format::format_command;
pub use model::{
    Color, CsaCommand, CsaCommandDirection, CsaMove, EndMarker, EndMarkerKind, GameSummary,
    InfoLine, MetaCommand, MoveLine, SpecialMove, TimeConfig, TimeScale, TimeUnit, TurnPreference,
};
pub use parser::{parse_line, parse_line_strict};
pub use summary::GameSummaryReader;
