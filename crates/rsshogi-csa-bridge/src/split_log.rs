//! Additional, append-only session and game event files.
//!
//! A valid manifest selects this layout explicitly. Consumers must not merge it
//! with the legacy run log. A game result and the writer's completion notice are
//! separate: only an index snapshot with `file_complete = true` seals a file.
//! Complete JSONL lines are recoverable after a process crash; no power-loss
//! durability is promised (`flush` is not `fsync`). Engine and probe stay shared.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::record::{GameResult, LogRecord, Phase, ProposalEndReason, RecordBody};

/// Stable identifier of the additional layout (independent of event schema 1).
pub const FORMAT: &str = "rsshogi-csa-split-events";
/// The first split-layout version.
pub const VERSION: u32 = 1;
const CAPACITY: usize = 128;
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// Immutable paths, relative to the manifest's directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub run_id: String,
    pub session_events: String,
    pub games_index: String,
    pub legacy_events: String,
    pub engine_log: String,
    pub probe_log: String,
}

/// Proposal lifecycle, independent of both the result and file completion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Proposed,
    Started,
    Rejected,
    InterruptedBeforeStart,
    Interrupted,
    Finished,
}

/// Explicit cause of a terminal index update.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    GameEnd,
    ValidationRejected,
    StopRequested,
    Rejected,
    Disconnected,
    SessionShutdown,
}

impl From<ProposalEndReason> for EndReason {
    fn from(reason: ProposalEndReason) -> Self {
        match reason {
            ProposalEndReason::ValidationRejected => Self::ValidationRejected,
            ProposalEndReason::StopRequested => Self::StopRequested,
            ProposalEndReason::Rejected => Self::Rejected,
            ProposalEndReason::Disconnected => Self::Disconnected,
            ProposalEndReason::SessionShutdown => Self::SessionShutdown,
        }
    }
}

/// Full index snapshot for one recorded `game_start`.
///
/// `game_index` counts all proposals from zero, including rejected ones. Latest
/// snapshots replace earlier ones for the same `(run_id, game_start_seq)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameEntry {
    pub run_id: String,
    pub game_index: u64,
    pub game_start_seq: u64,
    pub game_id: Option<String>,
    pub file: String,
    pub proposal_state: ProposalState,
    pub result: Option<GameResult>,
    pub end_reason: Option<EndReason>,
    pub end_event_seq: Option<u64>,
    pub last_event_seq: u64,
    pub file_complete: bool,
}

/// Append-only index records. Diagnostics do not consume a game index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IndexRecord {
    Game(GameEntry),
    Diagnostic {
        run_id: String,
        event_seq: u64,
        game_index: Option<u64>,
        game_start_seq: Option<u64>,
        game_id: Option<String>,
        reason: EndReason,
        detail: Option<String>,
    },
}

/// Recoverable index prefix. An unfinished last line is never a completion.
#[derive(Debug)]
pub struct IndexPrefix {
    pub records: Vec<IndexRecord>,
    pub trailing_bytes: usize,
}

/// Reads complete index lines, rejecting corrupt complete lines instead of
/// silently repairing them or falling back to the legacy run log.
pub fn read_index_prefix(bytes: &[u8]) -> io::Result<IndexPrefix> {
    let end = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
    let records = bytes[..end]
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| serde_json::from_slice(line).map_err(io::Error::other))
        .collect::<io::Result<Vec<_>>>()?;
    Ok(IndexPrefix { records, trailing_bytes: bytes.len() - end })
}

pub(crate) struct FramedEvent {
    pub record: LogRecord,
    /// Exactly the same envelope (including original seq and ts) as legacy.
    pub bytes: Vec<u8>,
}

/// Independent bounded writer; submission never awaits a file or queue space.
pub(crate) struct SplitWriter {
    tx: Option<mpsc::Sender<FramedEvent>>,
    task: Option<JoinHandle<io::Result<()>>>,
    failed: Arc<AtomicBool>,
    notify: mpsc::UnboundedSender<()>,
}

impl SplitWriter {
    pub async fn start(
        dir: &Path,
        run_id: u64,
        notify: mpsc::UnboundedSender<()>,
        committed: mpsc::UnboundedSender<u64>,
    ) -> io::Result<Self> {
        let log = SplitLog::create(dir, run_id).await?;
        Ok(Self::spawn(log, notify, committed))
    }

    fn spawn(
        mut log: SplitLog,
        notify: mpsc::UnboundedSender<()>,
        committed: mpsc::UnboundedSender<u64>,
    ) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::channel::<FramedEvent>(CAPACITY);
        let failure = failed.clone();
        let warning = notify.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let terminal_seq = matches!(
                    &event.record.body,
                    RecordBody::GameEnd { .. } | RecordBody::ProposalEnd { .. }
                )
                .then_some(event.record.seq);
                if failure.load(Ordering::Acquire) {
                    return Err(io::Error::other("split output lost an event"));
                }
                if let Err(err) = log.write(event, &failure).await {
                    failure.store(true, Ordering::Release);
                    eprintln!("FATAL: split event log failed: {err}; legacy log continues");
                    let _ = warning.send(());
                    return Err(err);
                }
                if let Some(seq) = terminal_seq {
                    let _ = committed.send(seq);
                }
            }
            // No synthetic seal: an unclosed proposal stays explicitly incomplete.
            Ok(())
        });
        Self { tx: Some(tx), task: Some(task), failed, notify }
    }

    pub fn submit(&self, event: FramedEvent) {
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        if self.tx.as_ref().expect("writer is open").try_send(event).is_err()
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            eprintln!("FATAL: split event queue unavailable; legacy log continues");
            let _ = self.notify.send(());
        }
    }

    pub async fn finish(mut self) -> io::Result<()> {
        self.tx.take();
        let mut task = self.task.take().expect("writer has a task");
        let result = match tokio::time::timeout(FINISH_TIMEOUT, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => Err(io::Error::other(format!("split writer panicked: {err}"))),
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                let _ = self.notify.send(());
                task.abort();
                let _ = task.await;
                Err(io::Error::new(io::ErrorKind::TimedOut, "split writer drain timed out"))
            }
        };
        result?;
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("split output is incomplete"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn with_read_only_index(
        dir: &Path,
        run_id: u64,
        notify: mpsc::UnboundedSender<()>,
    ) -> io::Result<Self> {
        let mut log = SplitLog::create(dir, run_id).await?;
        log.index = tokio::fs::File::open(log.root.join("games.jsonl")).await?;
        Ok(Self::spawn(log, notify, mpsc::unbounded_channel().0))
    }

    #[cfg(test)]
    pub(crate) async fn with_terminal_index_failure(
        dir: &Path,
        run_id: u64,
        notify: mpsc::UnboundedSender<()>,
        committed: mpsc::UnboundedSender<u64>,
    ) -> io::Result<Self> {
        let mut log = SplitLog::create(dir, run_id).await?;
        log.completion_index_read_only = true;
        Ok(Self::spawn(log, notify, committed))
    }
}

impl Drop for SplitWriter {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

struct ActiveGame {
    file: tokio::fs::File,
    entry: GameEntry,
}

#[derive(PartialEq, Eq)]
enum EntryUpdate {
    Unchanged,
    Started,
    Ended,
}

impl GameEntry {
    fn observe(&mut self, record: &LogRecord) -> EntryUpdate {
        self.last_event_seq = record.seq;
        match &record.body {
            RecordBody::State { phase: Phase::Playing } => {
                self.proposal_state = ProposalState::Started;
                self.game_id.clone_from(&record.game_id);
                return EntryUpdate::Started;
            }
            RecordBody::GameEnd { result, .. } => {
                if self.proposal_state == ProposalState::Started {
                    self.result = Some(*result);
                    self.proposal_state = ProposalState::Finished;
                } else {
                    self.proposal_state = ProposalState::InterruptedBeforeStart;
                }
                self.end_reason = Some(EndReason::GameEnd);
            }
            RecordBody::ProposalEnd { reason, .. } => {
                self.proposal_state = if self.proposal_state == ProposalState::Started {
                    ProposalState::Interrupted
                } else if *reason == ProposalEndReason::Rejected {
                    ProposalState::Rejected
                } else {
                    ProposalState::InterruptedBeforeStart
                };
                self.end_reason = Some((*reason).into());
            }
            _ => return EntryUpdate::Unchanged,
        }
        self.end_event_seq = Some(record.seq);
        EntryUpdate::Ended
    }
}

struct SplitLog {
    root: PathBuf,
    run_id: String,
    session: tokio::fs::File,
    index: tokio::fs::File,
    active: Option<ActiveGame>,
    next_index: u64,
    #[cfg(test)]
    crash_before_seal: bool,
    #[cfg(test)]
    completion_index_read_only: bool,
}

async fn create_file(path: &Path) -> io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new().write(true).create_new(true).open(path).await
}

async fn append(file: &mut tokio::fs::File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes).await?;
    file.flush().await
}

async fn index_record(file: &mut tokio::fs::File, record: &IndexRecord) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    bytes.push(b'\n');
    append(file, &bytes).await
}

impl SplitLog {
    async fn create(dir: &Path, run_id: u64) -> io::Result<Self> {
        let root = dir.join(format!("{run_id}-split"));
        tokio::fs::create_dir(&root).await?;
        tokio::fs::create_dir(root.join("games")).await?;
        let session = create_file(&root.join("session.jsonl")).await?;
        let index = create_file(&root.join("games.jsonl")).await?;
        let manifest = Manifest {
            format: FORMAT.to_owned(),
            version: VERSION,
            run_id: run_id.to_string(),
            session_events: "session.jsonl".to_owned(),
            games_index: "games.jsonl".to_owned(),
            legacy_events: format!("../{run_id}-events.jsonl"),
            engine_log: format!("../{run_id}-engine.log"),
            probe_log: format!("../{run_id}-probe.jsonl"),
        };
        let mut marker = create_file(&root.join("manifest.json")).await?;
        let mut bytes = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
        bytes.push(b'\n');
        append(&mut marker, &bytes).await?;
        Ok(Self {
            root,
            run_id: run_id.to_string(),
            session,
            index,
            active: None,
            next_index: 0,
            #[cfg(test)]
            crash_before_seal: false,
            #[cfg(test)]
            completion_index_read_only: false,
        })
    }

    async fn write(&mut self, event: FramedEvent, failed: &AtomicBool) -> io::Result<()> {
        let FramedEvent { record, bytes } = event;
        if matches!(record.body, RecordBody::GameStart { .. }) {
            if self.active.is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "previous game not closed"));
            }
            let file = format!("games/{}-{}.jsonl", self.next_index, record.seq);
            let mut output = create_file(&self.root.join(&file)).await?;
            // The file and its first complete event exist before advertising it.
            append(&mut output, &bytes).await?;
            let entry = GameEntry {
                run_id: self.run_id.clone(),
                game_index: self.next_index,
                game_start_seq: record.seq,
                game_id: record.game_id,
                file,
                proposal_state: ProposalState::Proposed,
                result: None,
                end_reason: None,
                end_event_seq: None,
                last_event_seq: record.seq,
                file_complete: false,
            };
            index_record(&mut self.index, &IndexRecord::Game(entry.clone())).await?;
            self.next_index += 1;
            self.active = Some(ActiveGame { file: output, entry });
            return Ok(());
        }

        if let RecordBody::ProposalEnd { game_start_seq: None, proposal_game_id, reason, detail } =
            &record.body
        {
            append(&mut self.session, &bytes).await?;
            return index_record(
                &mut self.index,
                &IndexRecord::Diagnostic {
                    run_id: self.run_id.clone(),
                    event_seq: record.seq,
                    game_index: None,
                    game_start_seq: None,
                    game_id: proposal_game_id.clone(),
                    reason: (*reason).into(),
                    detail: detail.clone(),
                },
            )
            .await;
        }

        let session_record = matches!(
            &record.body,
            RecordBody::BridgeStart { .. }
                | RecordBody::BridgeStop
                | RecordBody::Heartbeat
                | RecordBody::EngineMeta { .. }
                | RecordBody::EngineGeneration { .. }
                | RecordBody::Connection { .. }
                | RecordBody::State {
                    phase: Phase::Warming
                        | Phase::Connecting
                        | Phase::WaitingPairing
                        | Phase::Idle
                        | Phase::Closing
                }
        );
        if self.active.is_none()
            && matches!(
                &record.body,
                RecordBody::ProposalEnd { game_start_seq: Some(_), .. }
                    | RecordBody::GameEnd { .. }
            )
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "closure without an open game"));
        }
        if session_record || self.active.is_none() {
            if matches!(record.body, RecordBody::BridgeStop) && self.active.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session ended without proposal closure",
                ));
            }
            return append(&mut self.session, &bytes).await;
        }

        let active = self.active.as_mut().expect("an active game exists");
        if let RecordBody::ProposalEnd { game_start_seq: Some(seq), .. } = &record.body {
            if *seq != active.entry.game_start_seq {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "proposal closure identity mismatch",
                ));
            }
        }
        append(&mut active.file, &bytes).await?;
        let update = active.entry.observe(&record);
        if update != EntryUpdate::Unchanged {
            index_record(&mut self.index, &IndexRecord::Game(active.entry.clone())).await?;
        }
        if update == EntryUpdate::Ended {
            self.seal(failed).await?;
        }
        Ok(())
    }

    async fn seal(&mut self, failed: &AtomicBool) -> io::Result<()> {
        #[cfg(test)]
        if self.crash_before_seal {
            return Err(io::Error::other("simulated process crash before seal"));
        }
        // Switch away from the game, flush and close it, then advertise completion.
        let ActiveGame { mut file, mut entry } = self.active.take().expect("game is open");
        file.flush().await?;
        drop(file);
        #[cfg(test)]
        if self.completion_index_read_only {
            self.index = tokio::fs::File::open(self.root.join("games.jsonl")).await?;
        }
        if failed.load(Ordering::Acquire) {
            return Err(io::Error::other("cannot seal after dropped events"));
        }
        entry.file_complete = true;
        index_record(&mut self.index, &IndexRecord::Game(entry)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{AlertCode, AlertLevel, ConnectionEvent, ProposalEndReason};
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    async fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rsshogi-csa-split-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir(&dir).await.unwrap();
        dir
    }

    async fn cleanup(dir: PathBuf) {
        assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with("rsshogi-csa-split-"));
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    fn game_start() -> RecordBody {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/csa-event-log-contract.jsonl"
        ))
        .lines()
        .find_map(|line| {
            let record: LogRecord = serde_json::from_str(line).unwrap();
            matches!(record.body, RecordBody::GameStart { .. }).then_some(record.body)
        })
        .unwrap()
    }

    fn frame(seq: u64, body: RecordBody) -> FramedEvent {
        let record =
            LogRecord { seq, at: seq * 10, game_id: Some("reused/unsafe-id".into()), body };
        let mut value = serde_json::to_value(&record).unwrap();
        value["schema"] = 1.into();
        value["ts"] = (1000 + seq).into();
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        FramedEvent { record, bytes }
    }

    fn ending(start: Option<u64>, reason: ProposalEndReason) -> RecordBody {
        RecordBody::ProposalEnd {
            game_start_seq: start,
            proposal_game_id: Some("reused/unsafe-id".into()),
            reason,
            detail: Some("fixture".into()),
        }
    }

    fn game_end(result: GameResult) -> RecordBody {
        RecordBody::GameEnd {
            result,
            terminal: vec!["#LOSE".into()],
            black_remaining_ms: 1,
            white_remaining_ms: 2,
        }
    }

    async fn index(root: &Path) -> Vec<IndexRecord> {
        read_index_prefix(&tokio::fs::read(root.join("games.jsonl")).await.unwrap())
            .unwrap()
            .records
    }

    #[tokio::test]
    async fn reused_ids_rejections_and_late_events_keep_identity_and_exact_envelopes() {
        let dir = temp_dir().await;
        let mut log = SplitLog::create(&dir, 42).await.unwrap();
        let root = log.root.clone();
        let failed = AtomicBool::new(false);
        let mut expected = Vec::new();
        let records = [
            ending(None, ProposalEndReason::ValidationRejected),
            game_start(),
            ending(Some(2), ProposalEndReason::Rejected),
            game_start(),
            RecordBody::State { phase: Phase::Playing },
            game_end(GameResult::Lose),
            RecordBody::Alert {
                level: AlertLevel::Warn,
                code: AlertCode::StaleBestmove,
                detail: "late answer".into(),
            },
            RecordBody::EngineGeneration { generation: 2 },
            RecordBody::Connection { event: ConnectionEvent::Disconnected },
        ];
        for (i, body) in records.into_iter().enumerate() {
            let event = frame(i as u64 + 1, body);
            expected.push(event.bytes.clone());
            log.write(event, &failed).await.unwrap();
        }
        let entries = index(&root).await;
        assert!(matches!(
            &entries[0],
            IndexRecord::Diagnostic { event_seq: 1, game_index: None, game_start_seq: None, .. }
        ));
        let games: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                IndexRecord::Game(game) if game.file_complete => Some(game),
                _ => None,
            })
            .collect();
        assert_eq!(games.len(), 2);
        assert_eq!((games[0].game_index, games[0].game_start_seq), (0, 2));
        assert_eq!((games[1].game_index, games[1].game_start_seq), (1, 4));
        assert_eq!(games[0].proposal_state, ProposalState::Rejected);
        assert_eq!(games[0].result, None);
        assert_eq!(games[1].result, Some(GameResult::Lose));
        assert_eq!(games[0].game_id, games[1].game_id);
        assert_eq!(
            tokio::fs::read(root.join(&games[0].file)).await.unwrap(),
            expected[1..3].concat()
        );
        assert_eq!(
            tokio::fs::read(root.join(&games[1].file)).await.unwrap(),
            expected[3..6].concat()
        );
        assert_eq!(
            tokio::fs::read(root.join("session.jsonl")).await.unwrap(),
            [expected[0].clone(), expected[6..].concat()].concat()
        );
        let manifest: Manifest =
            serde_json::from_slice(&tokio::fs::read(root.join("manifest.json")).await.unwrap())
                .unwrap();
        assert_eq!(manifest.format, FORMAT);
        assert_eq!(manifest.version, VERSION);
        assert_eq!(manifest.run_id, "42");
        assert!(!games[0].file.contains("unsafe"));
        // All terminal snapshots remain explicitly unsealed until writer closure.
        assert!(entries.iter().any(|entry| matches!(entry, IndexRecord::Game(game)
            if game.result.is_some() && !game.file_complete)));
        drop(log);
        cleanup(dir).await;
    }

    #[tokio::test]
    async fn interrupted_proposals_and_started_games_have_different_states() {
        for started in [false, true] {
            for reason in [ProposalEndReason::Disconnected, ProposalEndReason::SessionShutdown] {
                let dir = temp_dir().await;
                let mut log = SplitLog::create(&dir, 1).await.unwrap();
                let failed = AtomicBool::new(false);
                log.write(frame(10, game_start()), &failed).await.unwrap();
                if started {
                    log.write(frame(11, RecordBody::State { phase: Phase::Playing }), &failed)
                        .await
                        .unwrap();
                }
                log.write(frame(12, ending(Some(10), reason)), &failed).await.unwrap();
                let entries = index(&log.root).await;
                let IndexRecord::Game(last) = entries.last().unwrap() else { panic!("game entry") };
                assert_eq!(
                    last.proposal_state,
                    if started {
                        ProposalState::Interrupted
                    } else {
                        ProposalState::InterruptedBeforeStart
                    }
                );
                assert_eq!(last.result, None);
                assert_eq!(last.end_event_seq, Some(12));
                assert!(last.file_complete);
                drop(log);
                cleanup(dir).await;
            }
        }
    }

    #[tokio::test]
    async fn resumed_start_keeps_replayed_moves_clock_and_original_sequence() {
        let dir = temp_dir().await;
        let mut log = SplitLog::create(&dir, 1).await.unwrap();
        let history = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/csa-event-log-contract.jsonl"
        ))
        .lines()
        .find_map(|line| {
            let record: LogRecord = serde_json::from_str(line).unwrap();
            if let RecordBody::Move(played) = record.body {
                Some(played)
            } else {
                None
            }
        })
        .unwrap();
        let mut body = game_start();
        let RecordBody::GameStart { moves_so_far, .. } = &mut body else { unreachable!() };
        moves_so_far.push(history);
        let event = frame(117, body);
        let expected = event.bytes.clone();
        log.write(event, &AtomicBool::new(false)).await.unwrap();
        let actual = tokio::fs::read(log.root.join("games/0-117.jsonl")).await.unwrap();
        assert_eq!(actual, expected);
        let start: serde_json::Value = serde_json::from_slice(&actual).unwrap();
        assert_eq!(start["seq"], 117);
        assert_eq!(start["moves_so_far"].as_array().unwrap().len(), 1);
        assert!(start["time"].is_object());
        assert!(start["initial_sfen"].is_string());
        drop(log);
        cleanup(dir).await;
    }

    #[tokio::test]
    async fn result_without_completion_survives_crash_and_partial_index_line() {
        let dir = temp_dir().await;
        let mut log = SplitLog::create(&dir, 1).await.unwrap();
        let root = log.root.clone();
        let failed = AtomicBool::new(false);
        log.write(frame(1, game_start()), &failed).await.unwrap();
        log.write(frame(2, RecordBody::State { phase: Phase::Playing }), &failed).await.unwrap();
        log.crash_before_seal = true;
        assert!(log.write(frame(3, game_end(GameResult::Lose)), &failed).await.is_err());
        drop(log);
        let mut index_file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(root.join("games.jsonl"))
            .await
            .unwrap();
        append(&mut index_file, b"{\"type\":\"game\"").await.unwrap();
        drop(index_file);
        let prefix =
            read_index_prefix(&tokio::fs::read(root.join("games.jsonl")).await.unwrap()).unwrap();
        assert!(prefix.trailing_bytes > 0);
        let IndexRecord::Game(last) = prefix.records.last().unwrap() else {
            panic!("game snapshot")
        };
        assert_eq!(last.result, Some(GameResult::Lose));
        assert!(!last.file_complete);
        assert!(tokio::fs::read_to_string(root.join(&last.file))
            .await
            .unwrap()
            .contains("game_end"));
        assert!(read_index_prefix(b"{corrupt}\n").is_err());
        assert!(read_index_prefix(b"\n").is_err(), "a corrupt complete line must not be skipped");
        cleanup(dir).await;
    }

    #[tokio::test]
    async fn completed_game_file_with_failed_completion_notice_is_not_sealed() {
        let dir = temp_dir().await;
        let mut log = SplitLog::create(&dir, 1).await.unwrap();
        let failed = AtomicBool::new(false);
        log.write(frame(1, game_start()), &failed).await.unwrap();
        log.write(frame(2, RecordBody::State { phase: Phase::Playing }), &failed).await.unwrap();
        log.completion_index_read_only = true;
        assert!(log.write(frame(3, game_end(GameResult::Win)), &failed).await.is_err());
        assert!(log.active.is_none(), "writer has actually closed the game file");
        let entries = index(&log.root).await;
        let IndexRecord::Game(last) = entries.last().unwrap() else { panic!("snapshot") };
        assert_eq!(last.result, Some(GameResult::Win));
        assert!(!last.file_complete);
        drop(log);
        cleanup(dir).await;
    }

    #[tokio::test]
    async fn existing_game_file_is_never_overwritten_or_advertised() {
        let dir = temp_dir().await;
        let mut log = SplitLog::create(&dir, 1).await.unwrap();
        let target = log.root.join("games/0-11.jsonl");
        tokio::fs::write(&target, b"existing artifact\n").await.unwrap();
        assert!(log.write(frame(11, game_start()), &AtomicBool::new(false)).await.is_err());
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"existing artifact\n");
        assert!(index(&log.root).await.is_empty());
        drop(log);
        cleanup(dir).await;
    }

    #[tokio::test]
    async fn full_queue_reports_failure_without_waiting_or_sealing() {
        let dir = temp_dir().await;
        let (notify, mut failures) = mpsc::unbounded_channel();
        let writer =
            SplitWriter::start(&dir, 1, notify, mpsc::unbounded_channel().0).await.unwrap();
        // No await: this single-thread runtime cannot poll the writer yet.
        for seq in 1..=CAPACITY as u64 + 1 {
            writer.submit(frame(seq, RecordBody::Heartbeat));
        }
        assert!(writer.failed.load(Ordering::Acquire));
        assert_eq!(failures.try_recv(), Ok(()));
        assert!(writer.finish().await.is_err());
        cleanup(dir).await;
    }
}
