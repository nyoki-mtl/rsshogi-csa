//! Loss-tolerant, bounded contextual search diagnostics, isolated from match logging.

use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rsshogi_usi::{InfoCommand, MateScore, ScoreBound, ScoreValue};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::{io::AsyncWriteExt, sync::mpsc, task::JoinHandle};

const MAX_LINE: usize = 64 * 1024;
const MAX_STRING: usize = 4 * 1024;
const MAX_PV: usize = 512;
const QUEUE_CAPACITY: usize = 256;

/// Immutable search identity and the state at the instant this record was classified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchContext {
    pub game_start_seq: String,
    pub game_id: String,
    pub search_id: String,
    pub root_sfen: String,
    pub root_side: crate::record::Side,
    pub base_ply: u32,
    pub predicted_usi: Option<String>,
    pub origin: String,
    pub state: String,
    pub ponder_outcome: Option<String>,
}

/// A core-produced record; envelope identity and wall time belong to the writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeRecord {
    pub at: u64,
    pub context: Option<SearchContext>,
    #[serde(flatten)]
    pub body: Value,
}

fn prefix(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Convert parsed USI fields without inventing absent values or flattening mate scores.
#[must_use]
pub fn info_body(info: &InfoCommand) -> Value {
    let mut fields = Map::new();
    let mut truncated = Vec::new();
    macro_rules! number {
        ($field:ident, $name:literal) => {
            if let Some(value) = info.$field {
                fields.insert($name.into(), json!(value));
            }
        };
    }
    number!(depth, "depth");
    number!(seldepth, "seldepth");
    number!(time, "time_ms");
    number!(nodes, "nodes");
    number!(multipv, "multipv");
    number!(hashfull, "hashfull");
    number!(nps, "nps");
    if let Some(score) = &info.score {
        let bound = match score.bound {
            Some(ScoreBound::Lower) => "lower",
            Some(ScoreBound::Upper) => "upper",
            None => "exact",
        };
        let (kind, value) = match &score.value {
            ScoreValue::Cp(cp) => ("cp", json!(cp)),
            ScoreValue::Mate(mate) => (
                "mate",
                match mate {
                    MateScore::Ply(ply) => json!({"kind":"plies", "value":ply}),
                    MateScore::UnknownWin => json!({"kind":"unknown_distance", "sign":"positive"}),
                    MateScore::UnknownLose => json!({"kind":"unknown_distance", "sign":"negative"}),
                },
            ),
        };
        fields.insert(
            "score".into(),
            json!({"kind":kind, "value":value, "bound":bound, "perspective":"root_side"}),
        );
    }
    for (name, value) in [("currmove", &info.currmove), ("string", &info.string)] {
        if let Some(value) = value {
            let bounded = prefix(value, MAX_STRING);
            if bounded.len() != value.len() {
                truncated.push(name);
            }
            fields.insert(name.into(), json!(bounded));
        }
    }
    if !info.pv.is_empty() {
        // Valid USI moves have at most five bytes. Bound even malformed codec input.
        let pv: Vec<_> = info.pv.iter().take(MAX_PV).map(|mv| prefix(mv, 64)).collect();
        if info.pv.len() > MAX_PV || info.pv.iter().take(MAX_PV).any(|mv| mv.len() > 64) {
            truncated.push("pv");
        }
        fields.insert("pv".into(), json!(pv));
    }
    json!({"type":"info", "info":fields, "truncated_fields":truncated})
}

struct LimitedBytes(Vec<u8>);
impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_LINE - 1 {
            return Err(io::Error::other("probe record exceeds line limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode(value: &impl Serialize) -> Option<Vec<u8>> {
    let mut bytes = LimitedBytes(Vec::new());
    serde_json::to_writer(&mut bytes, value).ok()?;
    Some(bytes.0)
}

fn warn_once(warned: &AtomicBool) {
    if !warned.swap(true, Ordering::Relaxed) {
        eprintln!("optional probe output omitted (queue, size, or I/O limit)");
    }
}

fn redact_strings(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            // Match the original text once: replacement text must not itself be
            // matched by another secret, and overlapping secrets use the longest.
            let mut masked = String::new();
            let mut remaining = text.as_str();
            while !remaining.is_empty() {
                if let Some(secret) = secrets
                    .iter()
                    .filter(|secret| !secret.is_empty() && remaining.starts_with(secret.as_str()))
                    .max_by_key(|secret| secret.len())
                {
                    masked.push_str("[REDACTED]");
                    remaining = &remaining[secret.len()..];
                } else {
                    let ch = remaining.chars().next().expect("nonempty string");
                    masked.push(ch);
                    remaining = &remaining[ch.len_utf8()..];
                }
            }
            *text = masked;
        }
        Value::Array(items) => {
            for item in items {
                redact_strings(item, secrets);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                redact_strings(item, secrets);
            }
        }
        _ => {}
    }
}

/// Independent optional writer. Submitting diagnostics never waits for disk or queue space.
pub struct ProbeWriter {
    run_id: String,
    seq: u64,
    tx: mpsc::Sender<Vec<u8>>,
    task: JoinHandle<()>,
    warned: Arc<AtomicBool>,
}

impl ProbeWriter {
    /// Create a fresh run file; failures disable only this diagnostic stream.
    pub async fn start(log_dir: &Path, run_id: u64, redact: Vec<String>) -> Option<Self> {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o640);
        let Ok(file) = options.open(log_dir.join(format!("{run_id}-probe.jsonl"))).await else {
            eprintln!("optional probe output could not be created");
            return None;
        };
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let warned = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(write_records(file, rx, redact, warned.clone()));
        Some(Self { run_id: run_id.to_string(), seq: 0, tx, task, warned })
    }

    /// Consume a logical sequence number even when this lossy record cannot be queued.
    pub fn submit(&mut self, engine_generation: u64, record: ProbeRecord) {
        #[derive(Serialize)]
        struct Envelope<'a> {
            schema: u8,
            run_id: &'a str,
            probe_seq: String,
            engine_generation: String,
            #[serde(flatten)]
            record: ProbeRecord,
        }
        self.seq = self.seq.saturating_add(1);
        let envelope = Envelope {
            schema: 1,
            run_id: &self.run_id,
            probe_seq: self.seq.to_string(),
            engine_generation: engine_generation.to_string(),
            record,
        };
        if let Some(bytes) = encode(&envelope) {
            if self.tx.try_send(bytes).is_ok() {
                return;
            }
        }
        warn_once(&self.warned);
    }

    /// Drain briefly on shutdown without making match shutdown depend on diagnostics.
    pub async fn finish(self) {
        drop(self.tx);
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(1), &mut task).await.is_err() {
            task.abort();
            warn_once(&self.warned);
        }
    }
}

async fn write_records(
    mut file: tokio::fs::File,
    mut rx: mpsc::Receiver<Vec<u8>>,
    redact: Vec<String>,
    warned: Arc<AtomicBool>,
) {
    while let Some(bytes) = rx.recv().await {
        let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
            warn_once(&warned);
            continue;
        };
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        value["ts"] = json!(u64::try_from(millis).unwrap_or(u64::MAX));
        redact_strings(&mut value, &redact);
        let Some(mut bytes) = encode(&value) else {
            warn_once(&warned);
            continue;
        };
        bytes.push(b'\n');
        if file.write_all(&bytes).await.is_err() {
            warn_once(&warned);
            return;
        }
    }
    if file.flush().await.is_err() {
        warn_once(&warned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsshogi_usi::InfoScore;
    use std::sync::atomic::AtomicU64;

    fn record(body: Value) -> ProbeRecord {
        ProbeRecord { at: 123, context: None, body }
    }

    fn temp_dir() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "csa-probe-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn structured_info_preserves_scores_and_missing_fields() {
        for (mate, value) in [
            (MateScore::Ply(-5), json!({"kind":"plies", "value":-5})),
            (MateScore::Ply(0), json!({"kind":"plies", "value":0})),
            (MateScore::Ply(7), json!({"kind":"plies", "value":7})),
            (MateScore::UnknownWin, json!({"kind":"unknown_distance", "sign":"positive"})),
            (MateScore::UnknownLose, json!({"kind":"unknown_distance", "sign":"negative"})),
        ] {
            let info = InfoCommand::new()
                .with_multipv(3)
                .with_score(InfoScore::mate(mate).with_lowerbound());
            let body = info_body(&info);
            assert_eq!(
                body["info"]["score"],
                json!({"kind":"mate", "value":value, "bound":"lower", "perspective":"root_side"})
            );
            assert_eq!(body["info"]["multipv"], 3);
            assert!(body["info"].get("depth").is_none());
        }
        let cp = info_body(&InfoCommand::new().with_score(InfoScore::cp(-32).with_upperbound()));
        assert_eq!(cp["info"]["score"]["bound"], "upper");
        assert_eq!(cp["info"]["score"]["value"], -32);
        assert_eq!(
            info_body(&InfoCommand::new().with_score_cp(0))["info"]["score"]["bound"],
            "exact"
        );
        assert!(info_body(&InfoCommand::string("hello"))["info"].get("multipv").is_none());
    }

    #[test]
    fn utf8_and_pv_are_bounded_before_ownership() {
        let info = InfoCommand {
            string: Some("棋".repeat(2000)),
            pv: vec!["7g7f".into(); 600],
            ..InfoCommand::default()
        };
        let body = info_body(&info);
        assert_eq!(body["info"]["string"].as_str().unwrap().len(), 4095);
        assert_eq!(body["info"]["pv"].as_array().unwrap().len(), MAX_PV);
        assert_eq!(body["truncated_fields"], json!(["string", "pv"]));
    }

    #[tokio::test]
    async fn writer_persists_envelope_and_redacts_decoded_strings() {
        let dir = temp_dir();
        let secret = "quote\"slash\\newline\n棋";
        let mut writer = ProbeWriter::start(&dir, 42, vec![secret.into()]).await.unwrap();
        let context = SearchContext {
            game_start_seq: "8".into(),
            game_id: secret.into(),
            search_id: "9".into(),
            root_sfen: "9/9/9/9/9/9/9/9/9 b - 1".into(),
            root_side: crate::record::Side::Black,
            base_ply: 0,
            predicted_usi: None,
            origin: "normal".into(),
            state: "thinking".into(),
            ponder_outcome: None,
        };
        writer.submit(7, ProbeRecord { at: 123, context: Some(context), body: json!({"type":"info", "info":{"string":format!("before {secret} after"), "pv":[secret]}}) });
        writer.finish().await;
        let bytes = std::fs::read(dir.join("42-probe.jsonl")).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(bytes.len() <= MAX_LINE);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["run_id"], "42");
        assert_eq!(value["probe_seq"], "1");
        assert_eq!(value["engine_generation"], "7");
        assert_eq!(value["at"], 123);
        assert!(value["ts"].as_u64().unwrap() > 0);
        assert_eq!(value["context"]["game_id"], "[REDACTED]");
        assert_eq!(value["info"]["string"], "before [REDACTED] after");
        assert_eq!(value["info"]["pv"][0], "[REDACTED]");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("42-probe.jsonl")).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saturation_is_nonblocking_and_consumes_sequence_numbers() {
        let (tx, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let mut writer = ProbeWriter {
            run_id: "1".into(),
            seq: 0,
            tx,
            task: tokio::spawn(async {}),
            warned: Arc::new(AtomicBool::new(false)),
        };
        // No receiver runs during these calls: an awaiting send would deadlock this test.
        for _ in 0..=QUEUE_CAPACITY {
            writer.submit(1, record(json!({"type":"search"})));
        }
        assert_eq!(writer.tx.capacity(), 0);
        assert!(writer.warned.load(Ordering::Relaxed));
        rx.recv().await.unwrap();
        writer.submit(1, record(json!({"type":"search"})));
        let mut last = Value::Null;
        while let Ok(bytes) = rx.try_recv() {
            last = serde_json::from_slice(&bytes).unwrap();
        }
        assert_eq!(last["probe_seq"], "258");
        // Oversize strings are rejected by streaming serialization, without queueing them.
        writer.submit(1, record(json!({"type":"info", "string":"x".repeat(MAX_LINE * 2)})));
        assert!(rx.try_recv().is_err());
        writer.submit(1, record(json!({"type":"engine"})));
        let after: Value = serde_json::from_slice(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(after["probe_seq"], "260");
        drop(rx);
        writer.submit(1, record(json!({"type":"engine"})));
        writer.finish().await;
    }

    #[tokio::test]
    async fn creation_failure_is_optional_and_never_overwrites() {
        let dir = temp_dir();
        assert!(ProbeWriter::start(&dir.join("missing"), 1, vec![]).await.is_none());
        std::fs::write(dir.join("1-probe.jsonl"), "original").unwrap();
        assert!(ProbeWriter::start(&dir, 1, vec![]).await.is_none());
        assert_eq!(std::fs::read_to_string(dir.join("1-probe.jsonl")).unwrap(), "original");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn write_failure_does_not_escape_writer() {
        let dir = temp_dir();
        let path = dir.join("read-only");
        std::fs::write(&path, "original").unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(serde_json::to_vec(&json!({"type":"engine"})).unwrap()).unwrap();
        drop(tx);
        let warned = Arc::new(AtomicBool::new(false));
        write_records(file, rx, vec![], warned.clone()).await;
        assert!(warned.load(Ordering::Relaxed));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
