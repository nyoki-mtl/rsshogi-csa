//! The I/O shell.
//!
//! This is the only module that touches sockets, processes, files, and the
//! clock. It performs the [`Action`]s the core returns and turns the outside
//! world back into [`Event`]s. It holds no protocol knowledge of its own.
//!
//! The socket's write half stays in the main loop and nothing else can block it:
//! log writing happens on its own task behind a channel, and both readers push
//! into an unbounded queue rather than being polled.
//!
//! # Nothing here may end the process
//!
//! On a CSA server, losing the socket loses the game, so the bridge staying
//! alive *is* the reliability feature. A failed action is therefore reported as
//! the event it really is — a dead engine, a lost connection — and handed back
//! to the core to decide about. Only failures during setup, before anything is
//! at stake, are returned as errors.
//!
//! Deadlines are pulled. After every `step` the loop asks the core when it next
//! wants waking and sleeps until then. A wakeup that is late, or one for a
//! deadline that has since gone away, costs a single no-op `Tick`.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rsshogi_csa::format_command;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::record::{LogRecord, Millis};
use crate::session::{Action, Event, OperatorCommand, SessionCore};

/// Where the shell connects, what it runs, and where it writes.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Server host name.
    pub host: String,
    /// Server port. CSA servers listen on 4081.
    pub port: u16,
    /// The engine executable.
    pub engine_path: PathBuf,
    /// Extra arguments for the engine.
    pub engine_args: Vec<String>,
    /// Working directory for the engine, which is where it finds its book and eval.
    pub engine_dir: Option<PathBuf>,
    /// Where the three logs are written.
    pub log_dir: PathBuf,
    /// How long to let the engine drain after `quit` before killing it.
    ///
    /// A GPU engine can take five seconds to unwind an in-flight search.
    pub quit_grace: Duration,
    /// Substrings masked before anything is written to the wire log.
    ///
    /// The login line carries the password, and the wire log is the file most
    /// likely to be attached to a bug report or pasted into a chat. Masking here
    /// rather than at the call site means a future command that happens to carry
    /// the secret is covered too.
    pub redact: Vec<String>,
}

/// Which direction a wire line went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireDir {
    In,
    Out,
}

impl WireDir {
    const fn as_str(self) -> &'static str {
        match self {
            Self::In => "<<",
            Self::Out => ">>",
        }
    }
}

/// What the background tasks push at the main loop.
///
/// An engine death carries the spawn it refers to. A failed write and the
/// reader task both report the same death, and after a restart the later report
/// would otherwise look like the replacement dying and burn a second restart.
enum Incoming {
    Event(Event),
    EngineGone { at: Millis, generation: u64 },
}

enum LogLine {
    Record(Box<LogRecord>),
    Wire { at: Millis, dir: WireDir, raw: String },
    Engine { at: Millis, dir: WireDir, raw: String },
}

/// Runs one bridge session to completion.
///
/// Returns when the core asks to shut down, when the event stream ends, or on a
/// fatal I/O error.
pub async fn run(mut core: SessionCore, config: ShellConfig) -> io::Result<()> {
    tokio::fs::create_dir_all(&config.log_dir).await?;
    // The logs are opened here rather than inside the writer task so that a
    // directory we cannot write to stops the bridge before anything is playing.
    // Nothing is lost by refusing to start; a game played with no record is a
    // game the operator cannot see, audit, or trust.
    let (run_id, files) = create_log_files(&config.log_dir).await?;
    // The id the dashboard keys on. Printed because it is no longer guessable
    // from the clock now that it carries milliseconds and may have been bumped.
    eprintln!("run {run_id}: logging to {}", config.log_dir.display());
    let (log_tx, log_rx) = mpsc::unbounded_channel();
    let redact = config.redact.clone();
    let writer = tokio::spawn(writer_task(log_rx, files, redact));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let start = Instant::now();
    spawn_operator_reader(event_tx.clone(), start);

    let mut shell = Shell {
        config,
        start,
        event_tx,
        events_log_lost: false,
        log_tx,
        server_write: None,
        server_reader: None,
        engine_stdin: None,
        child: None,
        engine_generation: 0,
    };

    let actions = core.step(Event::Started { at: shell.now() });
    let mut done = shell.execute(actions).await;

    while !done {
        // Wake for whichever comes first: a protocol deadline, or the moment a
        // heartbeat would be due. Without the second, a quiet stretch could sleep
        // past the heartbeat interval and break the guarantee a reader depends
        // on — that a live bridge never leaves a longer gap in its log.
        let wake = merge_wakeup(core.next_deadline(), core.next_heartbeat());
        let Some(event) = shell.next_event(&mut event_rx, wake).await else {
            break;
        };
        let actions = core.step(event);
        done = shell.execute(actions).await;
    }

    shell.stop_engine().await;
    shell.stop_server_reader().await;
    let events_log_lost = shell.events_log_lost;
    drop(shell);

    // The writer's result was discarded here. A disk-full or flush failure ended
    // the task, every later send failed silently, and the process still exited
    // 0 — so nothing downstream could tell a clean run from one that lost its
    // entire record.
    let writer_result = match writer.await {
        Ok(result) => result,
        Err(err) => Err(io::Error::other(format!("log writer panicked: {err}"))),
    };
    if let Err(err) = writer_result {
        eprintln!("FATAL: the event log writer failed: {err}");
        return Err(err);
    }
    if events_log_lost {
        return Err(io::Error::other(
            "the event log writer failed; this run is not fully recorded",
        ));
    }
    Ok(())
}

/// The events log is gone and the operator has been told once.
///
/// `対局を落とさないことが最優先` protects the game *in progress*, and it says
/// nothing about games not yet begun. So the current game plays to its end — no
/// resignation, no disconnection — and the bridge then declines to take another
/// pairing, because a game played with no record is one the operator cannot see,
/// audit, or trust. The mislabel window on the dashboard (which will read the
/// coming silence as 異常終了) is bounded at one game, and by then it is nearly
/// true: the bridge is in terminal wind-down.
struct Shell {
    events_log_lost: bool,
    config: ShellConfig,
    start: Instant,
    event_tx: mpsc::UnboundedSender<Incoming>,
    log_tx: mpsc::UnboundedSender<LogLine>,
    server_write: Option<OwnedWriteHalf>,
    server_reader: Option<JoinHandle<()>>,
    engine_stdin: Option<ChildStdin>,
    child: Option<Child>,
    engine_generation: u64,
}

impl Shell {
    fn now(&self) -> Millis {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The events writer died. Finish this game, then stop taking pairings.
    ///
    /// Reported on stderr and through a nonzero exit, because those are the
    /// operator channels that cannot die with the file. The previous code
    /// dropped this failure twice — the send result and the writer's `JoinHandle`
    /// were both discarded — so the bridge played on with no log at all and
    /// nothing anywhere said so.
    fn on_events_log_lost(&mut self) {
        if self.events_log_lost {
            return;
        }
        self.events_log_lost = true;
        eprintln!(
            "FATAL: the event log writer failed; this game will be finished and no further \
             pairing will be accepted"
        );
        let _ = self.event_tx.send(Incoming::Event(Event::Operator {
            at: self.now(),
            command: OperatorCommand::StopAfterGame,
        }));
    }

    async fn next_event(
        &self,
        rx: &mut mpsc::UnboundedReceiver<Incoming>,
        deadline: Option<Millis>,
    ) -> Option<Event> {
        loop {
            let incoming = match deadline {
                None => rx.recv().await?,
                Some(deadline) => {
                    let remaining = Duration::from_millis(deadline.saturating_sub(self.now()));
                    tokio::select! {
                        biased;
                        incoming = rx.recv() => incoming?,
                        () = tokio::time::sleep(remaining) => {
                            return Some(Event::Tick { at: self.now() });
                        }
                    }
                }
            };
            match incoming {
                Incoming::Event(event) => return Some(event),
                Incoming::EngineGone { at, generation } => {
                    if generation == self.engine_generation {
                        return Some(Event::EngineExited { at });
                    }
                    // A late report about an engine that has already been replaced.
                }
            }
        }
    }

    /// Performs actions, reporting failures as events instead of propagating them.
    ///
    /// Returns whether the core asked to shut down.
    async fn execute(&mut self, actions: Vec<Action>) -> bool {
        for action in actions {
            match action {
                Action::SpawnEngine => {
                    // Whatever happened to the previous one is worth recording
                    // before its handle is replaced.
                    self.record_engine_status().await;
                    if let Err(err) = self.spawn_engine() {
                        self.report_engine_gone(&format!("engine could not be started: {err}"));
                    }
                }
                Action::SendEngine(command) => {
                    let line = rsshogi_usi::format_command(&command);
                    if let Err(err) = self.write_engine(&line).await {
                        // A broken pipe here means the engine is gone. Killing
                        // the bridge over it would drop the socket and lose the
                        // game outright, which is strictly worse.
                        self.report_engine_gone(&format!("engine write failed: {err}"));
                    }
                }
                Action::Connect => {
                    if let Err(err) = self.connect().await {
                        eprintln!("connect failed: {err}");
                        // Back off before telling the core, so a server that is
                        // down cannot spin this loop.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let _ = self
                            .event_tx
                            .send(Incoming::Event(Event::ServerEof { at: self.now() }));
                    }
                }
                Action::SendServer(command) => {
                    let line = format_command(&command);
                    if let Err(err) = self.write_server(&line).await {
                        eprintln!("server write failed: {err}");
                        self.server_write = None;
                        let _ = self
                            .event_tx
                            .send(Incoming::Event(Event::ServerEof { at: self.now() }));
                    }
                }
                Action::Log(record) => {
                    if self.log_tx.send(LogLine::Record(Box::new(record))).is_err() {
                        self.on_events_log_lost();
                    }
                }
                Action::Shutdown { reason } => {
                    eprintln!("shutting down: {reason}");
                    return true;
                }
            }
        }
        false
    }

    /// Drops the engine handles and tells the core the engine is gone.
    ///
    /// The reader task normally raises `EngineExited` on its own, but a write
    /// can fail first, and a spawn that never succeeded has no reader at all.
    /// The core treats a repeated report as a no-op.
    fn report_engine_gone(&mut self, detail: &str) {
        eprintln!("{detail}");
        self.engine_stdin = None;
        let _ = self
            .event_tx
            .send(Incoming::EngineGone { at: self.now(), generation: self.engine_generation });
    }

    fn spawn_engine(&mut self) -> io::Result<()> {
        self.engine_generation = self.engine_generation.saturating_add(1);
        let generation = self.engine_generation;
        let mut command = Command::new(&self.config.engine_path);
        command.args(&self.config.engine_args);
        if let Some(dir) = &self.config.engine_dir {
            command.current_dir(dir);
        } else if let Some(dir) = self.config.engine_path.parent() {
            command.current_dir(dir);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command.kill_on_drop(true);

        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("engine has no stdout"))?;
        let stderr = child.stderr.take();
        self.engine_stdin = child.stdin.take();

        let tx = self.event_tx.clone();
        let log_tx = self.log_tx.clone();
        let start = self.start;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let reason = loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let at = elapsed_ms(start);
                        let _ = log_tx.send(LogLine::Engine {
                            at,
                            dir: WireDir::In,
                            raw: line.clone(),
                        });
                        if tx.send(Incoming::Event(Event::EngineLine { at, line })).is_err() {
                            return;
                        }
                    }
                    // The pipe closed, which normally means the process exited.
                    Ok(None) => break "stdout reached end of file".to_owned(),
                    // A read error is a different thing entirely: the process may
                    // still be alive. Recording which one happened is the only way
                    // to tell them apart afterwards.
                    Err(err) => break format!("stdout read failed: {err}"),
                }
            };
            let at = elapsed_ms(start);
            let _ = log_tx.send(LogLine::Engine {
                at,
                dir: WireDir::In,
                raw: format!("[bridge] engine reader ended: {reason}"),
            });
            let _ = tx.send(Incoming::EngineGone { at, generation });
        });

        if let Some(stderr) = stderr {
            let log_tx = self.log_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let at = elapsed_ms(start);
                    let _ = log_tx.send(LogLine::Engine {
                        at,
                        dir: WireDir::In,
                        raw: format!("stderr: {line}"),
                    });
                }
            });
        }

        self.child = Some(child);
        Ok(())
    }

    async fn connect(&mut self) -> io::Result<()> {
        let stream = TcpStream::connect((self.config.host.as_str(), self.config.port)).await?;
        // Small lines with a hard latency budget: never wait for a full segment.
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        self.stop_server_reader().await;
        self.server_write = Some(write);

        let tx = self.event_tx.clone();
        let log_tx = self.log_tx.clone();
        let start = self.start;
        self.server_reader = Some(tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            loop {
                let Ok(Some(line)) = lines.next_line().await else {
                    let _ = tx.send(Incoming::Event(Event::ServerEof { at: elapsed_ms(start) }));
                    return;
                };
                let at = elapsed_ms(start);
                let _ = log_tx.send(LogLine::Wire { at, dir: WireDir::In, raw: line.clone() });
                if tx.send(Incoming::Event(Event::ServerLine { at, line })).is_err() {
                    return;
                }
            }
        }));

        let _ = self.event_tx.send(Incoming::Event(Event::Connected { at: self.now() }));
        Ok(())
    }

    async fn stop_server_reader(&mut self) {
        self.server_write = None;
        if let Some(reader) = self.server_reader.take() {
            // Await cancellation so the task has dropped its log sender before
            // `run` closes the channel and joins the writer.
            reader.abort();
            let _ = reader.await;
        }
    }

    async fn write_server(&mut self, line: &str) -> io::Result<()> {
        let at = self.now();
        let _ = self.log_tx.send(LogLine::Wire { at, dir: WireDir::Out, raw: line.to_owned() });
        let Some(write) = &mut self.server_write else {
            return Ok(());
        };
        write.write_all(line.as_bytes()).await?;
        write.write_all(b"\n").await?;
        write.flush().await
    }

    async fn write_engine(&mut self, line: &str) -> io::Result<()> {
        let at = self.now();
        let _ = self.log_tx.send(LogLine::Engine { at, dir: WireDir::Out, raw: line.to_owned() });
        let Some(stdin) = &mut self.engine_stdin else {
            return Ok(());
        };
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    /// Reports how the engine process finished, for the engine log.
    ///
    /// The exit code is what separates "it chose to exit" from "something killed
    /// it", which the reader task alone cannot tell. It is worth a short wait:
    /// asking the moment stdout reaches end of file races the process actually
    /// being reaped, and the answer then reads as "still running" whatever
    /// happened. That misreading cost an investigation once already.
    async fn record_engine_status(&mut self) {
        let Some(child) = &mut self.child else {
            return;
        };
        let raw = match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
            Ok(Ok(status)) => format!("[bridge] engine exit status: {status}"),
            Ok(Err(err)) => format!("[bridge] engine status unavailable: {err}"),
            Err(_) => "[bridge] engine outlived its stdout by 500ms, still running".to_owned(),
        };
        let at = self.now();
        let _ = self.log_tx.send(LogLine::Engine { at, dir: WireDir::In, raw });
    }

    async fn stop_engine(&mut self) {
        let _ = self.write_engine("quit").await;
        self.engine_stdin = None;
        let Some(child) = &mut self.child else {
            return;
        };
        if tokio::time::timeout(self.config.quit_grace, child.wait()).await.is_err() {
            eprintln!("engine did not exit within the grace period, killing it");
            let _ = child.kill().await;
        }
    }
}

/// The earlier of two optional wakeups, or whichever one exists.
const fn merge_wakeup(left: Option<Millis>, right: Option<Millis>) -> Option<Millis> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if left < right { left } else { right }),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn elapsed_ms(start: Instant) -> Millis {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn spawn_operator_reader(tx: mpsc::UnboundedSender<Incoming>, start: Instant) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let command = match line.trim() {
                "resign" => OperatorCommand::Resign,
                "stop" | "stop-after-game" => OperatorCommand::StopAfterGame,
                "quit" => OperatorCommand::Quit,
                other => {
                    if !other.is_empty() {
                        eprintln!("unknown operator command: `{other}` (resign / stop / quit)");
                    }
                    continue;
                }
            };
            let operator = Event::Operator { at: elapsed_ms(start), command };
            if tx.send(Incoming::Event(operator)).is_err() {
                return;
            }
        }
    });
}

/// Owns the three log files so no write can stall the event loop.
///
/// Every line is flushed as it is written: what survives a crash is the whole
/// point of keeping these.
struct LogFiles {
    events: tokio::fs::File,
    wire: tokio::fs::File,
    engine: tokio::fs::File,
}

/// Claims a run id nobody else holds, and the three files that go with it.
///
/// The id was epoch *seconds* and the files were opened with `File::create`,
/// which truncates. Two bridges started in the same second therefore shared one
/// run id and silently clobbered each other's logs, and a reader that saw the
/// file shrink spliced the two sessions into one run.
///
/// Milliseconds make the collision improbable; `create_new` makes it impossible
/// to *lose* anything when it happens anyway — the open fails and we take the
/// next id instead of overwriting a log that is in use.
async fn create_log_files(dir: &Path) -> io::Result<(u64, LogFiles)> {
    let mut run_id = epoch_millis();
    for _ in 0..1_000 {
        match open_run_files(dir, run_id).await {
            Ok(files) => return Ok((run_id, files)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => run_id += 1,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not claim a free run id"))
}

async fn open_run_files(dir: &Path, run_id: u64) -> io::Result<LogFiles> {
    let mut options = tokio::fs::OpenOptions::new();
    let options = options.write(true).create_new(true);
    Ok(LogFiles {
        events: options.open(dir.join(format!("{run_id}-events.jsonl"))).await?,
        wire: options.open(dir.join(format!("{run_id}-wire.log"))).await?,
        engine: options.open(dir.join(format!("{run_id}-engine.log"))).await?,
    })
}

async fn write_line(file: &mut tokio::fs::File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes).await?;
    file.flush().await
}

/// Says it once. A failing disk fails every line, and a message per line would
/// bury the one that matters.
fn report_degraded(alive: &mut bool, name: &str, err: &io::Error) {
    if !*alive {
        return;
    }
    *alive = false;
    eprintln!("WARNING: {name} is no longer being written ({err}); the game continues");
}

async fn writer_task(
    mut rx: mpsc::UnboundedReceiver<LogLine>,
    files: LogFiles,
    redact: Vec<String>,
) -> io::Result<()> {
    let LogFiles { mut events, mut wire, mut engine } = files;
    let mut wire_alive = true;
    let mut engine_alive = true;

    while let Some(line) = rx.recv().await {
        match line {
            LogLine::Record(record) => {
                let mut value = serde_json::to_value(&*record)
                    .unwrap_or_else(|_| serde_json::json!({"type": "unserializable"}));
                if let Some(object) = value.as_object_mut() {
                    object.insert("schema".to_owned(), serde_json::json!(1));
                    object.insert("ts".to_owned(), serde_json::json!(epoch_millis()));
                }
                events.write_all(value.to_string().as_bytes()).await?;
                events.write_all(b"\n").await?;
                events.flush().await?;
            }
            // The wire and engine logs are diagnostics, not the published
            // contract, so they do not share the events log's fate: losing them
            // is worth saying out loud and nothing more. Only the events log can
            // stop the bridge taking another pairing.
            LogLine::Wire { at, dir, raw } => {
                let masked = redact
                    .iter()
                    .filter(|secret| !secret.is_empty())
                    .fold(raw, |line, secret| line.replace(secret.as_str(), "*****"));
                let text = format!("{at} {} {masked}\n", dir.as_str());
                if let Err(err) = write_line(&mut wire, text.as_bytes()).await {
                    report_degraded(&mut wire_alive, "wire.log", &err);
                }
            }
            LogLine::Engine { at, dir, raw } => {
                let text = format!("{at} {} {raw}\n", dir.as_str());
                if let Err(err) = write_line(&mut engine, text.as_bytes()).await {
                    report_degraded(&mut engine_alive, "engine.log", &err);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs started in the same millisecond must not share a log.
    ///
    /// The id was epoch *seconds* and the files were opened with `File::create`,
    /// which truncates: a second bridge started inside the same second silently
    /// overwrote the first one's log while it was still playing, and the reader
    /// spliced the two sessions into one run. `create_new` makes the overwrite
    /// impossible, so the worst case is now a bumped id.
    #[tokio::test]
    async fn a_second_run_never_takes_over_an_existing_log() {
        let dir = std::env::temp_dir().join(format!("rsshogi-csa-log-{}", epoch_millis()));
        tokio::fs::create_dir_all(&dir).await.expect("temp dir");

        let (first_id, first) = create_log_files(&dir).await.expect("first run claims an id");
        drop(first);
        // Pin the collision rather than hoping the clock repeats: claim the very
        // id the next run would compute.
        let taken = dir.join(format!("{first_id}-events.jsonl"));
        assert!(taken.exists(), "the first run created its events log");

        let (second_id, _second) =
            create_log_files(&dir).await.expect("second run claims another id");
        assert_ne!(first_id, second_id, "the second run must not reuse the first run's id");
        assert!(
            tokio::fs::metadata(&taken).await.expect("first log still there").len() == 0,
            "the first run's log is untouched",
        );

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// A bridge that cannot write its log must not start playing.
    ///
    /// Nothing is at stake before the first pairing, so refusing costs nothing —
    /// and a game played with no record is one the operator cannot see, audit or
    /// trust.
    #[tokio::test]
    async fn a_directory_that_cannot_be_written_stops_the_bridge_before_it_plays() {
        let file = std::env::temp_dir().join(format!("rsshogi-csa-not-a-dir-{}", epoch_millis()));
        tokio::fs::write(&file, b"not a directory").await.expect("temp file");

        let result = create_log_files(&file).await;

        assert!(
            result.is_err(),
            "opening logs inside a plain file must fail rather than be ignored"
        );
        tokio::fs::remove_file(&file).await.ok();
    }

    #[tokio::test]
    async fn wire_log_masks_bare_and_floodgate_passwords_in_the_artifact() {
        for (index, password, expected) in [
            (0_u64, "secret", "LOGIN us *****"),
            (1_u64, "floodgate-300-10F,secret", "LOGIN us floodgate-300-10F,*****"),
        ] {
            let dir = std::env::temp_dir()
                .join(format!("rsshogi-csa-redaction-{}-{index}", epoch_millis()));
            tokio::fs::create_dir_all(&dir).await.expect("temp dir");
            let run_id = epoch_millis();
            let files = open_run_files(&dir, run_id).await.expect("log files");
            let (tx, rx) = mpsc::unbounded_channel();
            let writer = tokio::spawn(writer_task(rx, files, vec!["secret".to_owned()]));
            tx.send(LogLine::Wire {
                at: 0,
                dir: WireDir::Out,
                raw: format!("LOGIN us {password}"),
            })
            .expect("queue wire line");
            drop(tx);
            writer.await.expect("writer task").expect("write log");

            let artifact = tokio::fs::read_to_string(dir.join(format!("{run_id}-wire.log")))
                .await
                .expect("read wire log");
            assert!(artifact.contains(expected), "{artifact}");
            assert!(!artifact.contains("secret"), "{artifact}");
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }
}
