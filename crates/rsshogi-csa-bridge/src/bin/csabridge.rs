//! Plays a USI engine on a CSA protocol shogi server.
//!
//! ```text
//! csabridge --name myengine --password floodgate-300-10F,secret \
//!           --engine C:\engines\YaneuraOu.exe --games 1
//! ```
//!
//! While it runs, `resign`, `stop`, and `quit` on stdin are operator commands.

use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Parser};
use rsshogi_csa::TurnPreference;
use rsshogi_csa_bridge::config::{BridgeConfig, PanicPolicy};
use rsshogi_csa_bridge::session::SessionCore;
use rsshogi_csa_bridge::shell::{self, ShellConfig};

#[derive(Debug, Parser)]
#[command(name = "csabridge", version, about = "Play a USI engine on a CSA server")]
#[command(group(
    ArgGroup::new("credential")
        .required(true)
        .multiple(false)
        .args(["password", "password_env", "password_file"])
))]
#[allow(clippy::struct_field_names)]
struct Args {
    /// Server host.
    #[arg(long, default_value = "wdoor.c.u-tokyo.ac.jp")]
    host: String,

    /// Server port.
    #[arg(long, default_value_t = 4081)]
    port: u16,

    /// Login name. On floodgate this is the name that appears in the archive.
    #[arg(long)]
    name: String,

    /// Login password. In CSA mode it also carries the game name, as in
    /// `floodgate-300-10F,secret`.
    #[arg(long)]
    password: Option<String>,

    /// Environment variable containing the login password.
    ///
    /// In CSA mode the value also carries the game name, for example
    /// `floodgate-300-10F,secret`.
    #[arg(long)]
    password_env: Option<String>,

    /// UTF-8 file containing the login password.
    ///
    /// A trailing CR/LF is removed. Other bytes are significant.
    #[arg(long)]
    password_file: Option<PathBuf>,

    /// Use the `x1` extension, which keeps the connection open between games.
    #[arg(long)]
    x1: bool,

    /// Game name for the explicit `%%GAME` that `x1` mode needs.
    #[arg(long)]
    game_name: Option<String>,

    /// Requested turn: `+`, `-`, or `*`.
    #[arg(long, default_value = "*")]
    turn: char,

    /// The engine executable.
    #[arg(long)]
    engine: PathBuf,

    /// Working directory for the engine. Defaults to the executable's own directory.
    #[arg(long)]
    engine_dir: Option<PathBuf>,

    /// Extra arguments passed to the engine.
    #[arg(long = "engine-arg")]
    engine_args: Vec<String>,

    /// `setoption` pairs, as `Name=Value`. Repeatable.
    ///
    /// `USI_Ponder=true` is how pondering is turned on: the bridge drives
    /// `go ponder` whenever the engine offers a move to ponder on.
    #[arg(long = "setoption")]
    setoptions: Vec<String>,

    /// How many games to play. `0` keeps going until stopped.
    #[arg(long, default_value_t = 1)]
    games: u32,

    /// Milliseconds withheld from our own clock when telling the engine its budget.
    #[arg(long, default_value_t = 2_500)]
    margin_ms: u64,

    /// Milliseconds to wait for a `bestmove` after sending `stop`.
    #[arg(long, default_value_t = 1_000)]
    stop_grace_ms: u64,

    /// Blank-line keepalive interval. The protocol floor is 30 seconds.
    #[arg(long, default_value_t = 40_000)]
    keepalive_ms: u64,

    /// Log an alert when `bestmove resign` arrives above this score, in centipawns.
    ///
    /// Reporting only. The resign itself is always passed on.
    #[arg(long, default_value_t = 0)]
    resign_cp: i32,

    /// Resign after this many consecutive moves the engine failed to produce.
    #[arg(long, default_value_t = 2)]
    max_degraded_moves: u32,

    /// Play any legal move instead of resigning when the ladder runs out.
    #[arg(long)]
    any_legal_on_panic: bool,

    /// Do not attach `,'* <score> <pv...>` to our moves.
    #[arg(long)]
    no_score_comment: bool,

    /// How many principal variation moves to attach after the score. `0` for none.
    #[arg(long, default_value_t = 32)]
    pv_comment_moves: usize,

    /// Where the event, wire, and engine logs are written.
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    /// Seconds to let the engine drain after `quit` before killing it.
    #[arg(long, default_value_t = 6)]
    quit_grace_secs: u64,
}

fn nonempty_password(value: String, source: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{source} contains an empty password"));
    }
    Ok(value)
}

fn resolve_password(args: &Args) -> Result<String, String> {
    if let Some(value) = &args.password {
        return nonempty_password(value.clone(), "--password");
    }
    if let Some(name) = &args.password_env {
        let value = std::env::var(name)
            .map_err(|err| format!("failed to read password environment variable {name}: {err}"))?;
        return nonempty_password(value, &format!("environment variable {name}"));
    }
    if let Some(path) = &args.password_file {
        let raw = std::fs::read_to_string(path)
            .map_err(|err| format!("failed to read password file {}: {err}", path.display()))?;
        let value = raw.trim_end_matches(['\r', '\n']).to_owned();
        return nonempty_password(value, &format!("password file {}", path.display()));
    }
    Err("one of --password, --password-env or --password-file is required".to_owned())
}

fn login_secret(password: &str) -> &str {
    password.split_once(',').map_or(password, |(_, secret)| secret)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let password = resolve_password(&args)?;

    let turn = TurnPreference::from_csa(args.turn)
        .ok_or_else(|| format!("turn must be one of `+`, `-`, `*`, got `{}`", args.turn))?;

    // Nothing about pondering is decided here. The bridge always drives
    // `go ponder` when the engine offers a move to ponder on, and `USI_Ponder`
    // is what decides whether it offers one — so it is passed through like any
    // other engine option, and its default is the engine's own.
    let mut engine_options: Vec<(String, String)> = Vec::new();
    for pair in &args.setoptions {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("--setoption needs Name=Value, got `{pair}`"))?;
        engine_options.retain(|(existing, _)| existing != name);
        engine_options.push((name.to_owned(), value.to_owned()));
    }

    // Floodgate prefixes the secret with a game name. Other CSA servers use the
    // whole password as the secret.
    let secret = login_secret(&password).to_owned();

    let mut config = BridgeConfig::floodgate(args.name, password);
    config.x1 = args.x1;
    config.game_name = args.game_name;
    config.turn = turn;
    config.engine_options = engine_options;
    config.margin_ms = args.margin_ms;
    config.stop_grace_ms = args.stop_grace_ms;
    config.keepalive_interval_ms = args.keepalive_ms;
    config.resign_threshold_cp = args.resign_cp;
    config.max_degraded_moves = args.max_degraded_moves;
    config.panic_policy =
        if args.any_legal_on_panic { PanicPolicy::AnyLegal } else { PanicPolicy::Resign };
    config.send_score_comment = !args.no_score_comment;
    config.pv_comment_moves = args.pv_comment_moves;
    config.games = args.games;

    let engine_cmd = args.engine.display().to_string();
    let core = SessionCore::new(config, engine_cmd, env!("CARGO_PKG_VERSION"));

    let shell_config = ShellConfig {
        host: args.host,
        port: args.port,
        engine_path: args.engine,
        engine_args: args.engine_args,
        engine_dir: args.engine_dir,
        log_dir: args.log_dir,
        quit_grace: Duration::from_secs(args.quit_grace_secs),
        redact: vec![secret],
    };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(shell::run(core, shell_config))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_source_is_required_and_exclusive() {
        let base = ["csabridge", "--name", "us", "--engine", "engine.exe"];
        assert!(Args::try_parse_from(base).is_err());

        let direct = [
            "csabridge",
            "--name",
            "us",
            "--engine",
            "engine.exe",
            "--password",
            "floodgate-300-10F,secret",
        ];
        assert!(Args::try_parse_from(direct).is_ok());

        let both = [
            "csabridge",
            "--name",
            "us",
            "--engine",
            "engine.exe",
            "--password-env",
            "PASSWORD",
            "--password",
            "floodgate-300-10F,secret",
        ];
        assert!(Args::try_parse_from(both).is_err());
    }

    #[test]
    fn password_file_removes_only_trailing_line_endings() {
        let path =
            std::env::temp_dir().join(format!("rsshogi-csa-password-{}.txt", std::process::id()));
        std::fs::write(&path, " floodgate-300-10F,secret \r\n").expect("write password fixture");
        let args = Args::try_parse_from([
            "csabridge",
            "--name",
            "us",
            "--engine",
            "engine.exe",
            "--password-file",
            path.to_str().expect("UTF-8 temp path"),
        ])
        .expect("valid args");

        assert_eq!(resolve_password(&args).unwrap(), " floodgate-300-10F,secret ");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn login_secret_covers_bare_and_floodgate_passwords() {
        assert_eq!(login_secret("secret"), "secret");
        assert_eq!(login_secret("floodgate-300-10F,secret"), "secret");
    }
}
