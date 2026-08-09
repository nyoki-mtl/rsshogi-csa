//! Plays a USI engine on a CSA protocol shogi server.
//!
//! The crate is layered so that the parts that can lose a game are the parts
//! that can be tested without I/O:
//!
//! - [`clock`] reconstructs the server's own time bookkeeping from the `,T`
//!   values it echoes, and converts it to USI `go` parameters.
//! - [`game_state`] keeps the board and that ledger in one place, with a single
//!   mutation path, and owns the legality gate every outgoing move passes.
//! - [`session`] is the state machine: one `step(Event) -> Vec<Action>`, no I/O,
//!   no clock reading, no async runtime type.
//! - [`record`] is the event log the session emits, which is both the viewer's
//!   data source and the tournament's forensic trail.
//!
//! - [`shell`] is the only module that touches sockets, processes, files, and
//!   the clock. It performs actions and turns the world back into events.

pub mod clock;
pub mod config;
pub mod game_state;
pub mod record;
pub mod session;
pub mod shell;

pub use clock::{ClockError, ClockKind, ClockLedger, ClockRules, MoveBudget, UsiGoTimes};
pub use config::{usi_entering_king_rule, BridgeConfig, PanicPolicy};
pub use game_state::{entering_king_rule_for, GameState, GameStateError, PlayedMove};
pub use record::{
    AlertCode, AlertLevel, EvalRecord, GameResult, LogRecord, Millis, MoveOrigin, MoveRecord,
    Phase, PlayerNames, PonderOutcome, RecordBody, Side, TimeSummary,
};
pub use session::{Action, Event, OperatorCommand, SessionCore};
pub use shell::ShellConfig;
