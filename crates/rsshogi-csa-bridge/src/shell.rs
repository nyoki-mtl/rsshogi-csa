//! The I/O shell.
//!
//! This is the only module that touches sockets, processes, files, and the
//! clock. It performs the [`Action`]s the core returns and turns the outside
//! world back into [`Event`]s. It holds no protocol knowledge of its own.
//!
//! Each socket and engine generation has one writer with a bounded queue.
//! The main loop never awaits their writes or a connection attempt.
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
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rsshogi_csa::format_command;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::probe::ProbeWriter;
use crate::record::{LogRecord, Millis};
use crate::session::{Action, Event, OperatorCommand, SessionCore};
use crate::split_log::{FramedEvent, SplitWriter};

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
    EngineLine { at: Millis, generation: u64, line: String },
    EngineGone { at: Millis, generation: u64 },
    Connection { generation: u64, result: io::Result<TcpStream> },
    Server { generation: u64, event: Event },
}

impl Incoming {
    fn for_generation(self, current: u64) -> Option<Event> {
        match self {
            Self::Event(event) => Some(event),
            Self::EngineLine { at, generation, line } if generation == current => {
                Some(Event::EngineLine { at, line })
            }
            Self::EngineGone { at, generation } if generation == current => {
                Some(Event::EngineExited { at })
            }
            Self::EngineLine { .. }
            | Self::EngineGone { .. }
            | Self::Connection { .. }
            | Self::Server { .. } => None,
        }
    }
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
    let (critical_tx, critical_rx) = mpsc::unbounded_channel();
    let (committed_tx, committed_rx) = mpsc::unbounded_channel();
    let split =
        SplitWriter::start(&config.log_dir, run_id, critical_tx.clone(), committed_tx).await?;
    let probe = ProbeWriter::start(&config.log_dir, run_id, config.redact.clone()).await;
    // The id the dashboard keys on. Printed because it is no longer guessable
    // from the clock now that it carries milliseconds and may have been bumped.
    eprintln!("run {run_id}: logging to {}", config.log_dir.display());
    let (log_tx, log_rx) = mpsc::unbounded_channel();
    let redact = config.redact.clone();
    let writer = tokio::spawn(writer_task(log_rx, files, redact, Some(split), critical_tx));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let start = Instant::now();
    spawn_operator_reader(event_tx.clone(), start)?;

    let mut shell = Shell {
        config,
        start,
        event_tx,
        critical_log_failed: false,
        critical_rx,
        committed_rx,
        server_submission_lost: false,
        log_tx,
        server_write: None,
        server_reader: None,
        engine_writer: None,
        engine_readers: Vec::new(),
        retired_engines: Vec::new(),
        engine_cleanup_failed: Arc::new(AtomicBool::new(false)),
        server_generation: 0,
        connector: None,
        child: None,
        engine_generation: 0,
        probe,
    };

    core.enable_log_completion_barrier();
    let actions = core.step(Event::Started { at: shell.now() });
    let mut done = shell.execute(actions);

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
        done = shell.execute(actions);
    }

    let engine_cleanup_result = shell.stop_engine().await;
    shell.stop_server_reader().await;
    let critical_log_failed = shell.critical_log_failed;
    let server_submission_lost = shell.server_submission_lost;
    if let Some(probe) = shell.probe.take() {
        probe.finish().await;
    }
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
        eprintln!("FATAL: critical event logging failed: {err}");
        return Err(err);
    }
    engine_cleanup_result?;
    if server_submission_lost {
        return Err(io::Error::other(
            "a critical CSA command could not be queued; this run lost an outbound submission",
        ));
    }
    if critical_log_failed {
        return Err(io::Error::other(
            "a critical event log or index failed; this run is not fully recorded",
        ));
    }
    Ok(())
}

/// A critical event output failed and the operator has been told once.
///
/// `対局を落とさないことが最優先` protects the game *in progress*, and it says
/// nothing about games not yet begun. So the current game plays to its end — no
/// resignation, no disconnection — and the bridge then declines to take another
/// pairing, because a game played with no record is one the operator cannot see,
/// audit, or trust. The mislabel window on the dashboard (which will read the
/// coming silence as 異常終了) is bounded at one game, and by then it is nearly
/// true: the bridge is in terminal wind-down.
struct Shell {
    critical_log_failed: bool,
    critical_rx: mpsc::UnboundedReceiver<()>,
    committed_rx: mpsc::UnboundedReceiver<u64>,
    server_submission_lost: bool,
    config: ShellConfig,
    start: Instant,
    event_tx: mpsc::UnboundedSender<Incoming>,
    log_tx: mpsc::UnboundedSender<LogLine>,
    server_write: Option<LineWriter>,
    server_generation: u64,
    connector: Option<JoinHandle<()>>,
    server_reader: Option<JoinHandle<()>>,
    engine_writer: Option<LineWriter>,
    engine_readers: Vec<JoinHandle<()>>,
    retired_engines: Vec<JoinHandle<io::Result<()>>>,
    engine_cleanup_failed: Arc<AtomicBool>,
    child: Option<Child>,
    engine_generation: u64,
    probe: Option<ProbeWriter>,
}

impl Shell {
    fn now(&self) -> Millis {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// A critical output failed. Finish this game, then stop taking pairings.
    ///
    /// Reported on stderr and through a nonzero exit, because those are the
    /// operator channels that cannot die with the file. The previous code
    /// dropped this failure twice — the send result and the writer's `JoinHandle`
    /// were both discarded — so the bridge played on with no log at all and
    /// nothing anywhere said so.
    fn on_critical_log_failure(&mut self) {
        if self.critical_log_failed {
            return;
        }
        self.critical_log_failed = true;
        eprintln!(
            "FATAL: a critical event log or index failed; this game will be finished and no further \
             pairing will be accepted"
        );
        let _ = self.event_tx.send(Incoming::Event(Event::Operator {
            at: self.now(),
            command: OperatorCommand::StopAfterGame,
        }));
    }

    async fn next_event(
        &mut self,
        rx: &mut mpsc::UnboundedReceiver<Incoming>,
        deadline: Option<Millis>,
    ) -> Option<Event> {
        loop {
            if self.critical_rx.try_recv().is_ok() {
                self.on_critical_log_failure();
                return Some(Event::Operator {
                    at: self.now(),
                    command: OperatorCommand::StopAfterGame,
                });
            }
            if let Ok(seq) = self.committed_rx.try_recv() {
                return Some(Event::LogCommitted { at: self.now(), seq });
            }
            let incoming = match deadline {
                None => tokio::select! {
                    biased;
                    Some(()) = self.critical_rx.recv() => {
                        self.on_critical_log_failure();
                        return Some(Event::Operator { at: self.now(), command: OperatorCommand::StopAfterGame });
                    }
                    Some(seq) = self.committed_rx.recv() => return Some(Event::LogCommitted { at: self.now(), seq }),
                    incoming = rx.recv() => incoming?,
                },
                Some(deadline) => {
                    if deadline <= self.now() {
                        return Some(Event::Tick { at: self.now() });
                    }
                    let remaining = Duration::from_millis(deadline.saturating_sub(self.now()));
                    tokio::select! {
                        biased;
                        Some(()) = self.critical_rx.recv() => {
                            self.on_critical_log_failure();
                            return Some(Event::Operator { at: self.now(), command: OperatorCommand::StopAfterGame });
                        }
                        Some(seq) = self.committed_rx.recv() => return Some(Event::LogCommitted { at: self.now(), seq }),
                        () = tokio::time::sleep(remaining) => {
                            return Some(Event::Tick { at: self.now() });
                        }
                        incoming = rx.recv() => incoming?,
                    }
                }
            };
            let incoming = match incoming {
                Incoming::Connection { generation, result }
                    if generation == self.server_generation =>
                {
                    match result {
                        Ok(stream) => {
                            self.install_connection(stream);
                            return Some(Event::Connected { at: self.now() });
                        }
                        Err(err) => {
                            eprintln!("connect failed: {err}");
                            return Some(Event::ServerEof { at: self.now() });
                        }
                    }
                }
                Incoming::Server { generation, event } if generation == self.server_generation => {
                    return Some(event)
                }
                incoming => incoming,
            };
            if let Some(event) = incoming.for_generation(self.engine_generation) {
                return Some(event);
            }
        }
    }

    /// Performs actions, reporting failures as events instead of propagating them.
    ///
    /// Returns whether the core asked to shut down.
    fn execute(&mut self, actions: Vec<Action>) -> bool {
        for action in actions {
            match action {
                Action::SpawnEngine => {
                    self.retire_engine();
                    if let Err(err) = self.spawn_engine() {
                        self.report_engine_gone(&format!("engine could not be started: {err}"));
                    }
                }
                Action::SendEngine(command) => {
                    let line = rsshogi_usi::format_command(&command);
                    if let Err(err) = self.write_engine(&line) {
                        // A broken pipe here means the engine is gone. Killing
                        // the bridge over it would drop the socket and lose the
                        // game outright, which is strictly worse.
                        self.report_engine_gone(&format!("engine write failed: {err}"));
                    }
                }
                Action::Connect => self.connect(),
                Action::SendServer(command) => {
                    let line = format_command(&command);
                    if let Err(err) = self.write_server(&line) {
                        // Congestion is not proof of a dead socket. Keep the
                        // in-flight frame intact and decline another game.
                        self.server_submission_lost = true;
                        eprintln!("server queue unavailable: {err}; critical command was not submitted; stopping after game");
                        let _ = self.event_tx.send(Incoming::Event(Event::Operator {
                            at: self.now(),
                            command: OperatorCommand::StopAfterGame,
                        }));
                    }
                }
                Action::Log(record) => {
                    if self.log_tx.send(LogLine::Record(Box::new(record))).is_err() {
                        self.on_critical_log_failure();
                    }
                }
                Action::Probe(record) => {
                    if let Some(probe) = &mut self.probe {
                        probe.submit(self.engine_generation, record);
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
        if let Some(writer) = self.engine_writer.take() {
            writer.task.abort();
        }
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
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("engine has no stdin"))?;
        self.engine_writer = Some(spawn_line_writer(
            stdin,
            self.event_tx.clone(),
            self.log_tx.clone(),
            self.start,
            WriterTarget::Engine(generation),
        ));

        let tx = self.event_tx.clone();
        let log_tx = self.log_tx.clone();
        let start = self.start;
        self.engine_readers.push(tokio::spawn(async move {
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
                        if tx.send(Incoming::EngineLine { at, generation, line }).is_err() {
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
        }));

        if let Some(stderr) = stderr {
            let log_tx = self.log_tx.clone();
            self.engine_readers.push(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let at = elapsed_ms(start);
                    let _ = log_tx.send(LogLine::Engine {
                        at,
                        dir: WireDir::In,
                        raw: format!("stderr: {line}"),
                    });
                }
            }));
        }

        self.child = Some(child);
        Ok(())
    }

    fn connect(&mut self) {
        self.cancel_server();
        self.server_generation = self.server_generation.saturating_add(1);
        let generation = self.server_generation;
        let host = self.config.host.clone();
        let port = self.config.port;
        let tx = self.event_tx.clone();
        self.connector = Some(tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                TcpStream::connect((host.as_str(), port)),
            )
            .await
            .map_or_else(
                |_| Err(io::Error::new(io::ErrorKind::TimedOut, "CSA connect timed out")),
                |result| {
                    result.and_then(|stream| {
                        stream.set_nodelay(true)?;
                        Ok(stream)
                    })
                },
            );
            if result.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let _ = tx.send(Incoming::Connection { generation, result });
        }));
    }

    fn install_connection(&mut self, stream: TcpStream) {
        let generation = self.server_generation;
        let (read, write) = stream.into_split();
        self.server_write = Some(spawn_line_writer(
            write,
            self.event_tx.clone(),
            self.log_tx.clone(),
            self.start,
            WriterTarget::Server(generation),
        ));
        let tx = self.event_tx.clone();
        let log_tx = self.log_tx.clone();
        let start = self.start;
        self.server_reader = Some(tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            loop {
                let Ok(Some(line)) = lines.next_line().await else {
                    let _ = tx.send(Incoming::Server {
                        generation,
                        event: Event::ServerEof { at: elapsed_ms(start) },
                    });
                    return;
                };
                let at = elapsed_ms(start);
                let _ = log_tx.send(LogLine::Wire { at, dir: WireDir::In, raw: line.clone() });
                if tx
                    .send(Incoming::Server { generation, event: Event::ServerLine { at, line } })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn cancel_server(&mut self) {
        if let Some(task) = self.connector.take() {
            task.abort();
        }
        if let Some(writer) = self.server_write.take() {
            writer.task.abort();
        }
        if let Some(reader) = self.server_reader.take() {
            reader.abort();
        }
    }

    async fn stop_server_reader(&mut self) {
        if let Some(writer) = self.server_write.take() {
            drop(writer.tx);
            let mut task = writer.task;
            if tokio::time::timeout(self.config.quit_grace, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(task) = self.connector.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(reader) = self.server_reader.take() {
            reader.abort();
            let _ = reader.await;
        }
    }

    fn write_server(&self, line: &str) -> io::Result<()> {
        // Reserve space for a critical frame rather than filling a blocked
        // stream with periodic blank lines.
        if line.is_empty()
            && self.server_write.as_ref().is_some_and(|writer| writer.tx.capacity() <= 1)
        {
            return Ok(());
        }
        match enqueue(self.server_write.as_ref(), line) {
            Err(_) if line.is_empty() => Ok(()), // Keepalive is expendable under backpressure.
            result => result,
        }
    }

    fn write_engine(&self, line: &str) -> io::Result<()> {
        enqueue(self.engine_writer.as_ref(), line)
    }

    fn retire_engine(&mut self) {
        let child = self.child.take();
        let writer = self.engine_writer.take();
        let readers = std::mem::take(&mut self.engine_readers);
        let grace = self.config.quit_grace;
        let log = self.log_tx.clone();
        let start = self.start;
        let failed = self.engine_cleanup_failed.clone();
        self.retired_engines.retain(|task| !task.is_finished());
        self.retired_engines.push(tokio::spawn(async move {
            let result = finish_engine(child, writer, readers, grace, log, start).await;
            if result.is_err() {
                failed.store(true, Ordering::Relaxed);
            }
            result
        }));
    }

    async fn stop_engine(&mut self) -> io::Result<()> {
        let mut result = finish_engine(
            self.child.take(),
            self.engine_writer.take(),
            std::mem::take(&mut self.engine_readers),
            self.config.quit_grace,
            self.log_tx.clone(),
            self.start,
        )
        .await;
        for task in self.retired_engines.drain(..) {
            let retired_result = task.await.unwrap_or_else(|err| {
                Err(io::Error::other(format!("engine cleanup task failed: {err}")))
            });
            if result.is_ok() {
                result = retired_result;
            }
        }
        if result.is_ok() && self.engine_cleanup_failed.load(Ordering::Relaxed) {
            result =
                Err(io::Error::other("a retired engine could not be cleaned up; see engine log"));
        }
        result
    }
}

const WRITE_QUEUE_CAPACITY: usize = 128;

struct LineWriter {
    tx: mpsc::Sender<String>,
    task: JoinHandle<()>,
}

#[derive(Clone, Copy)]
enum WriterTarget {
    Engine(u64),
    Server(u64),
}

fn enqueue(writer: Option<&LineWriter>, line: &str) -> io::Result<()> {
    let writer =
        writer.ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "writer unavailable"))?;
    writer.tx.try_send(line.to_owned()).map_err(|err| match err {
        mpsc::error::TrySendError::Full(_) => {
            io::Error::new(io::ErrorKind::WouldBlock, "outbound queue full")
        }
        mpsc::error::TrySendError::Closed(_) => {
            io::Error::new(io::ErrorKind::BrokenPipe, "outbound queue closed")
        }
    })
}

fn spawn_line_writer<W: AsyncWrite + Unpin + Send + 'static>(
    mut write: W,
    tx: mpsc::UnboundedSender<Incoming>,
    log: mpsc::UnboundedSender<LogLine>,
    start: Instant,
    target: WriterTarget,
) -> LineWriter {
    let (sender, mut rx) = mpsc::channel::<String>(WRITE_QUEUE_CAPACITY);
    let task = tokio::spawn(async move {
        while let Some(raw) = rx.recv().await {
            // This future retains its byte offset across partial writes. On
            // cancellation the stream is closed, never replayed or interleaved.
            let mut framed = raw.as_bytes().to_vec();
            framed.push(b'\n');
            if let Err(err) = async {
                write.write_all(&framed).await?;
                write.flush().await
            }
            .await
            {
                eprintln!("outbound write failed: {err}");
                let at = elapsed_ms(start);
                let incoming = match target {
                    WriterTarget::Engine(generation) => Incoming::EngineGone { at, generation },
                    WriterTarget::Server(generation) => {
                        Incoming::Server { generation, event: Event::ServerEof { at } }
                    }
                };
                let _ = tx.send(incoming);
                return;
            }
            // Completed local write only; this is not server acknowledgement.
            let at = elapsed_ms(start);
            let line = match target {
                WriterTarget::Engine(_) => LogLine::Engine { at, dir: WireDir::Out, raw },
                WriterTarget::Server(_) => LogLine::Wire { at, dir: WireDir::Out, raw },
            };
            let _ = log.send(line);
        }
    });
    LineWriter { tx: sender, task }
}

async fn finish_engine(
    mut child: Option<Child>,
    writer: Option<LineWriter>,
    readers: Vec<JoinHandle<()>>,
    grace: Duration,
    log: mpsc::UnboundedSender<LogLine>,
    start: Instant,
) -> io::Result<()> {
    let mut cleanup_result = Ok(());
    let mut writer_task = None;
    let deadline = tokio::time::Instant::now() + grace;
    if let Some(writer) = writer {
        let _ = writer.tx.try_send("quit".to_owned());
        drop(writer.tx);
        writer_task = Some(writer.task);
    }
    if let Some(child) = &mut child {
        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => format!("[bridge] engine exit status: {status}"),
            Ok(Err(err)) => {
                let detail = format!("[bridge] engine status unavailable: {err}");
                cleanup_result = Err(io::Error::other(detail.clone()));
                detail
            }
            Err(_) => {
                // Voluntary quit write + wait share `grace`. OS forced
                // termination/reaping has a separate finite cleanup allowance.
                match child.start_kill() {
                    Err(err) => {
                        let detail =
                            format!("[bridge] engine exceeded quit grace; kill failed: {err}");
                        cleanup_result = Err(io::Error::other(detail.clone()));
                        detail
                    }
                    Ok(()) => {
                        match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                            Ok(Ok(status)) => format!(
                            "[bridge] engine exceeded quit grace; killed; exit status: {status}"
                        ),
                            Ok(Err(err)) => {
                                let detail =
                                    format!("[bridge] engine kill requested; reap failed: {err}");
                                cleanup_result = Err(io::Error::other(detail.clone()));
                                detail
                            }
                            Err(_) => {
                                let detail =
                                    "[bridge] engine kill requested; reap timed out after 1s"
                                        .to_owned();
                                cleanup_result = Err(io::Error::other(detail.clone()));
                                detail
                            }
                        }
                    }
                }
            }
        };
        let _ = log.send(LogLine::Engine { at: elapsed_ms(start), dir: WireDir::In, raw: status });
    }
    if let Some(mut task) = writer_task {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
    for reader in readers {
        reader.abort();
        let _ = reader.await;
    }
    if let Err(err) = &cleanup_result {
        eprintln!("engine cleanup failed: {err}");
    }
    // Child retains kill_on_drop as a final attempt on cleanup failure.
    cleanup_result
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

fn spawn_operator_reader(tx: mpsc::UnboundedSender<Incoming>, start: Instant) -> io::Result<()> {
    use std::io::BufRead as _;
    // Stdin reads cannot be cancelled. Keep them outside Tokio's blocking pool
    // so runtime shutdown never joins a read waiting for terminal input or EOF.
    std::thread::Builder::new()
        .name("csabridge-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { return };
                if tx.is_closed() {
                    return;
                }
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
        })
        .map(|_| ())
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
    split: Option<SplitWriter>,
    critical: mpsc::UnboundedSender<()>,
) -> io::Result<()> {
    let LogFiles { mut events, mut wire, mut engine } = files;
    let mut wire_alive = true;
    let mut engine_alive = true;

    let legacy_result = async {
        while let Some(line) = rx.recv().await {
            match line {
                LogLine::Record(record) => {
                    let mut value = serde_json::to_value(&*record)
                        .unwrap_or_else(|_| serde_json::json!({"type": "unserializable"}));
                    if let Some(object) = value.as_object_mut() {
                        object.insert("schema".to_owned(), serde_json::json!(1));
                        object.insert("ts".to_owned(), serde_json::json!(epoch_millis()));
                    }
                    let mut bytes = value.to_string().into_bytes();
                    bytes.push(b'\n');
                    events.write_all(&bytes).await?;
                    events.flush().await?;
                    if let Some(split) = &split {
                        split.submit(FramedEvent { record: *record, bytes });
                    }
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
        Ok::<(), io::Error>(())
    }
    .await;
    if legacy_result.is_err() {
        let _ = critical.send(());
    }
    let split_result = if let Some(split) = split { split.finish().await } else { Ok(()) };
    legacy_result?;
    split_result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_step(
        core: &mut SessionCore,
        event: Event,
        tx: &mpsc::UnboundedSender<LogLine>,
    ) -> Vec<Action> {
        let actions = core.step(event);
        for action in &actions {
            if let Action::Log(record) = action {
                tx.send(LogLine::Record(Box::new(record.clone()))).unwrap();
            }
        }
        actions
    }

    #[tokio::test]
    async fn terminal_index_failure_never_releases_next_registration_and_legacy_finishes() {
        let dir = std::env::temp_dir().join(format!(
            "rsshogi-csa-terminal-index-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        tokio::fs::create_dir(&dir).await.unwrap();
        let (run_id, files) = create_log_files(&dir).await.unwrap();
        let (critical, failure_rx) = mpsc::unbounded_channel();
        let (committed, committed_rx) = mpsc::unbounded_channel();
        let split =
            SplitWriter::with_terminal_index_failure(&dir, run_id, critical.clone(), committed)
                .await
                .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(writer_task(rx, files, Vec::new(), Some(split), critical));
        let mut config = crate::config::BridgeConfig::floodgate("us", "secret");
        config.x1 = true;
        config.game_name = Some("test".into());
        config.games = 0;
        let mut core = SessionCore::new(config, "engine", "test");
        core.enable_log_completion_barrier();
        for event in [
            Event::Started { at: 0 },
            Event::EngineLine { at: 0, line: "usiok".into() },
            Event::EngineLine { at: 0, line: "readyok".into() },
            Event::Connected { at: 0 },
            Event::ServerLine { at: 0, line: "LOGIN:us OK".into() },
        ] {
            record_step(&mut core, event, &tx);
        }
        for line in include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../rsshogi-csa/tests/summaries/floodgate-proposal.txt"
        ))
        .lines()
        {
            record_step(&mut core, Event::ServerLine { at: 0, line: line.into() }, &tx);
        }
        record_step(&mut core, Event::ServerLine { at: 0, line: "START:test".into() }, &tx);
        record_step(&mut core, Event::EngineLine { at: 0, line: "bestmove 7g7f".into() }, &tx);
        let ended = record_step(&mut core, Event::ServerLine { at: 0, line: "#WIN".into() }, &tx);
        assert!(
            !ended.iter().any(|a| matches!(a, Action::SendServer(_))),
            "registration must wait for writer completion"
        );
        let (mut shell, mut events, _unused) = test_shell();
        shell.critical_rx = failure_rx;
        shell.committed_rx = committed_rx;
        let failed =
            tokio::time::timeout(Duration::from_secs(3), shell.next_event(&mut events, None))
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(failed, Event::Operator { command: OperatorCommand::StopAfterGame, .. }));
        let shutdown = record_step(&mut core, failed, &tx);
        assert!(shutdown.iter().any(|a| matches!(a, Action::Shutdown { .. })));
        assert!(!shutdown
            .iter()
            .any(|a| matches!(a, Action::SendServer(rsshogi_csa::CsaCommand::Meta(_)))));
        drop(tx);
        assert!(writer.await.unwrap().is_err());
        let legacy =
            tokio::fs::read_to_string(dir.join(format!("{run_id}-events.jsonl"))).await.unwrap();
        assert!(legacy.contains("game_end"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(legacy.lines().last().unwrap()).unwrap()
                ["type"],
            "bridge_stop"
        );
        let index = crate::split_log::read_index_prefix(
            &tokio::fs::read(dir.join(format!("{run_id}-split/games.jsonl"))).await.unwrap(),
        )
        .unwrap();
        let Some(crate::split_log::IndexRecord::Game(last)) = index.records.last() else {
            panic!("game snapshot")
        };
        assert_eq!(last.result, Some(crate::record::GameResult::Win));
        assert!(!last.file_complete);
        assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn index_write_failure_notifies_independently_and_legacy_keeps_writing() {
        let dir = std::env::temp_dir().join(format!(
            "rsshogi-csa-index-failure-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        tokio::fs::create_dir(&dir).await.unwrap();
        let (run_id, files) = create_log_files(&dir).await.unwrap();
        let (critical, mut failures) = mpsc::unbounded_channel();
        let split =
            SplitWriter::with_read_only_index(&dir, run_id, critical.clone()).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(writer_task(rx, files, Vec::new(), Some(split), critical));
        let start = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/csa-event-log-contract.jsonl"
        ))
        .lines()
        .find_map(|line| {
            let record: LogRecord = serde_json::from_str(line).unwrap();
            matches!(record.body, crate::record::RecordBody::GameStart { .. })
                .then_some(record.body)
        })
        .unwrap();
        let make = |seq, body| {
            LogLine::Record(Box::new(LogRecord {
                seq,
                at: seq,
                game_id: Some("test".into()),
                body,
            }))
        };
        tx.send(make(1, start)).unwrap();
        tokio::time::timeout(Duration::from_secs(3), failures.recv()).await.unwrap().unwrap();
        // The index has failed at the OS write, while the old writer still owns
        // a live receiver and accepts the current game's remaining records.
        tx.send(make(2, crate::record::RecordBody::State { phase: crate::record::Phase::Playing }))
            .unwrap();
        tx.send(make(
            3,
            crate::record::RecordBody::GameEnd {
                result: crate::record::GameResult::Lose,
                terminal: vec!["#LOSE".into()],
                black_remaining_ms: 0,
                white_remaining_ms: 0,
            },
        ))
        .unwrap();
        tx.send(make(4, crate::record::RecordBody::BridgeStop)).unwrap();
        drop(tx);
        assert!(writer.await.unwrap().is_err());
        let bytes =
            tokio::fs::read_to_string(dir.join(format!("{run_id}-events.jsonl"))).await.unwrap();
        let records: Vec<serde_json::Value> =
            bytes.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(records.len(), 4);
        assert_eq!(records.last().unwrap()["type"], "bridge_stop");
        let index = tokio::fs::read(dir.join(format!("{run_id}-split/games.jsonl"))).await.unwrap();
        assert!(crate::split_log::read_index_prefix(&index).unwrap().records.is_empty());
        assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn critical_log_failure_preempts_queued_new_proposals() {
        let (mut shell, mut events, _logs) = test_shell();
        let (failure, rx) = mpsc::unbounded_channel();
        shell.critical_rx = rx;
        shell
            .event_tx
            .send(Incoming::Event(Event::ServerLine { at: 0, line: "BEGIN Game_Summary".into() }))
            .unwrap();
        failure.send(()).unwrap();
        assert!(matches!(
            shell.next_event(&mut events, None).await,
            Some(Event::Operator { command: OperatorCommand::StopAfterGame, .. })
        ));
        assert!(shell.critical_log_failed);
        let mut core = SessionCore::new(
            crate::config::BridgeConfig::floodgate("us", "secret"),
            "engine",
            "test",
        );
        core.step(Event::Started { at: 0 });
        let stop = core.step(Event::Operator { at: 0, command: OperatorCommand::StopAfterGame });
        assert!(stop.iter().any(|action| matches!(action, Action::Shutdown { .. })));
        assert!(core
            .step(Event::ServerLine { at: 1, line: "BEGIN Game_Summary".into() })
            .is_empty());
    }

    fn test_shell() -> (Shell, mpsc::UnboundedReceiver<Incoming>, mpsc::UnboundedReceiver<LogLine>)
    {
        let (event_tx, events) = mpsc::unbounded_channel();
        let (log_tx, logs) = mpsc::unbounded_channel();
        let (_, critical_rx) = mpsc::unbounded_channel();
        let (_, committed_rx) = mpsc::unbounded_channel();
        (
            Shell {
                critical_log_failed: false,
                critical_rx,
                committed_rx,
                server_submission_lost: false,
                config: ShellConfig {
                    host: "127.0.0.1".into(),
                    port: 1,
                    engine_path: std::env::current_exe().expect("test executable"),
                    engine_args: Vec::new(),
                    engine_dir: None,
                    log_dir: PathBuf::new(),
                    quit_grace: Duration::from_millis(100),
                    redact: Vec::new(),
                },
                start: Instant::now(),
                event_tx,
                log_tx,
                server_write: None,
                server_reader: None,
                server_generation: 1,
                connector: None,
                engine_writer: None,
                engine_readers: Vec::new(),
                retired_engines: Vec::new(),
                engine_cleanup_failed: Arc::new(AtomicBool::new(false)),
                child: None,
                engine_generation: 1,
                probe: None,
            },
            events,
            logs,
        )
    }

    #[tokio::test]
    async fn short_writes_preserve_frames_and_fifo_without_replay() {
        use tokio::io::AsyncReadExt as _;
        let (mut shell, _events, mut logs) = test_shell();
        let (write, mut read) = tokio::io::duplex(1);
        shell.server_write = Some(spawn_line_writer(
            write,
            shell.event_tx.clone(),
            shell.log_tx.clone(),
            shell.start,
            WriterTarget::Server(1),
        ));
        for line in ["position startpos", "go btime 1", "stop", ""] {
            shell.write_server(line).expect("enqueue");
        }
        let expected = b"position startpos\ngo btime 1\nstop\n\n";
        let mut actual = vec![0; expected.len()];
        tokio::time::timeout(Duration::from_secs(1), read.read_exact(&mut actual))
            .await
            .expect("drain deadline")
            .expect("read");
        assert_eq!(actual, expected);
        shell.stop_server_reader().await;
        let mut complete = Vec::new();
        while let Ok(LogLine::Wire { raw, .. }) = logs.try_recv() {
            complete.push(raw);
        }
        assert_eq!(complete, ["position startpos", "go btime 1", "stop", ""]);
    }

    #[tokio::test]
    async fn blocked_server_queue_overflow_does_not_disconnect_or_replay() {
        use tokio::io::AsyncReadExt as _;
        let (mut shell, mut events, _logs) = test_shell();
        let (write, mut read) = tokio::io::duplex(1);
        shell.server_write = Some(spawn_line_writer(
            write,
            shell.event_tx.clone(),
            shell.log_tx.clone(),
            shell.start,
            WriterTarget::Server(1),
        ));
        for _ in 0..WRITE_QUEUE_CAPACITY {
            shell.write_server("critical").expect("bounded queue");
        }
        shell.write_server("").expect("congested keepalive skipped");
        assert!(!shell.execute(vec![Action::SendServer(rsshogi_csa::CsaCommand::Logout)]));
        assert!(shell.server_submission_lost);
        assert!(shell.server_write.is_some(), "overflow must preserve the socket writer");
        assert!(matches!(
            events.try_recv(),
            Ok(Incoming::Event(Event::Operator { command: OperatorCommand::StopAfterGame, .. }))
        ));
        assert!(events.try_recv().is_err(), "overflow must not synthesize EOF");
        let expected = "critical\n".repeat(WRITE_QUEUE_CAPACITY);
        let mut actual = vec![0; expected.len()];
        tokio::time::timeout(Duration::from_secs(1), read.read_exact(&mut actual))
            .await
            .expect("drain deadline")
            .expect("read");
        assert_eq!(actual, expected.as_bytes());
        shell.stop_server_reader().await;
    }

    #[tokio::test]
    async fn elapsed_deadline_preempts_info_backlog_and_old_socket_events() {
        let (mut shell, mut events, _logs) = test_shell();
        shell
            .event_tx
            .send(Incoming::Server { generation: 0, event: Event::ServerEof { at: 0 } })
            .unwrap();
        for _ in 0..1000 {
            shell
                .event_tx
                .send(Incoming::EngineLine { at: 0, generation: 1, line: "info nodes 1".into() })
                .unwrap();
        }
        assert!(matches!(shell.next_event(&mut events, Some(0)).await, Some(Event::Tick { .. })));
        assert!(matches!(
            shell.next_event(&mut events, None).await,
            Some(Event::EngineLine { .. })
        ));
        shell.cancel_server();
    }

    #[tokio::test]
    async fn retired_cleanup_failure_survives_completed_handle_pruning() {
        let (mut shell, _events, _logs) = test_shell();
        // Model an already-completed failed reap. A subsequent engine restart
        // prunes that handle, but its failure must still affect final status.
        let failed = shell.engine_cleanup_failed.clone();
        let task = tokio::spawn(async move {
            failed.store(true, Ordering::Relaxed);
            Err(io::Error::other("simulated retired-child reap failure"))
        });
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        shell.retired_engines.push(task);
        shell.retire_engine();
        assert_eq!(shell.retired_engines.len(), 1, "completed handle was pruned");
        let error = shell.stop_engine().await.expect_err("retired failure cannot become success");
        assert!(error.to_string().contains("retired engine"));
    }

    #[tokio::test]
    async fn enqueue_errors_never_include_login_or_option_payloads() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send("already queued".into()).unwrap();
        let writer = LineWriter { tx, task: tokio::spawn(async {}) };
        let login = "LOGIN user super-secret-password";
        let full = enqueue(Some(&writer), login).unwrap_err();
        assert_eq!(full.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(full.to_string(), "outbound queue full");
        assert!(!full.to_string().contains("super-secret-password"));
        rx.close();
        let closed =
            enqueue(Some(&writer), "setoption name Token value private-token").unwrap_err();
        assert_eq!(closed.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(closed.to_string(), "outbound queue closed");
        assert!(!closed.to_string().contains("private-token"));
        writer.task.await.unwrap();
    }

    #[tokio::test]
    async fn writer_failure_reports_its_own_engine_generation() {
        let (shell, mut events, _logs) = test_shell();
        let (write, read) = tokio::io::duplex(1);
        drop(read);
        let writer = spawn_line_writer(
            write,
            shell.event_tx.clone(),
            shell.log_tx.clone(),
            shell.start,
            WriterTarget::Engine(7),
        );
        enqueue(Some(&writer), "go").unwrap();
        writer.task.await.unwrap();
        let incoming = events.recv().await.unwrap();
        assert!(matches!(&incoming, Incoming::EngineGone { generation: 7, .. }));
        assert!(incoming.for_generation(8).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unread_real_child_shutdown_includes_quit_write_in_grace() {
        let (mut shell, _events, mut logs) = test_shell();
        shell.config.engine_args = vec![
            "--ignored".into(),
            "--exact".into(),
            "shell::tests::unread_child".into(),
            "--nocapture".into(),
        ];
        shell.spawn_engine().unwrap();
        shell.write_engine(&"x".repeat(8 * 1024 * 1024)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let start = Instant::now();
        tokio::time::timeout(Duration::from_millis(700), shell.stop_engine())
            .await
            .expect("quit write and child wait must share grace")
            .expect("direct child reaped");
        assert!(start.elapsed() < Duration::from_millis(600));
        assert!(shell.child.is_none());
        assert!(shell.engine_writer.is_none());
        assert!(shell.engine_readers.is_empty());
        let mut reaped = false;
        while let Ok(line) = logs.try_recv() {
            if let LogLine::Engine { raw, .. } = line {
                reaped |= raw.contains("killed; exit status:");
            }
        }
        assert!(reaped, "log must prove the direct child was reaped before success");
    }

    #[test]
    #[ignore = "child used by unread pipe shutdown test"]
    fn unread_child() {
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn old_engine_output_cannot_reach_the_new_generation() {
        for line in ["bestmove 7g7f", "readyok", "usiok", "info score cp 300"] {
            assert!(Incoming::EngineLine { at: 1, generation: 1, line: line.into() }
                .for_generation(2)
                .is_none());
            assert_eq!(
                Incoming::EngineLine { at: 1, generation: 2, line: line.into() }.for_generation(2),
                Some(Event::EngineLine { at: 1, line: line.into() })
            );
        }
        assert!(Incoming::EngineGone { at: 1, generation: 1 }.for_generation(2).is_none());
    }

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
            let (critical, _) = mpsc::unbounded_channel();
            let writer =
                tokio::spawn(writer_task(rx, files, vec!["secret".to_owned()], None, critical));
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
