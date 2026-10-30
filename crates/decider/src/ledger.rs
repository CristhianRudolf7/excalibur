//! Durable record of decisions and their eventual labels.
//!
//! The ledger stores only a hash of the decision input, never the input text,
//! so user content does not leak into calibration data. Labels come from the
//! harness itself: user corrections, task outcomes or explicit review.
use crate::{CalibrationReport, DeciderError, DecisionPort, LabeledOutcome, Result, Thresholds, Verdict};
use excalibur_store::{
    Database, DatabaseKind, Priority, ReadLane, StorageConfig,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS decider_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS decider_records(id TEXT PRIMARY KEY,port TEXT NOT NULL,question TEXT NOT NULL,kind TEXT NOT NULL,backend TEXT NOT NULL,probability REAL NOT NULL CHECK(probability>=0 AND probability<=1),verdict TEXT NOT NULL,selected TEXT,input_hash TEXT NOT NULL,created_ms INTEGER NOT NULL,label INTEGER CHECK(label IS NULL OR label IN (0,1)),label_source TEXT,labeled_ms INTEGER);
CREATE INDEX IF NOT EXISTS decider_records_port ON decider_records(port,question,label);
CREATE TABLE IF NOT EXISTS decider_thresholds(port TEXT NOT NULL,question TEXT NOT NULL,tau_low REAL,tau_high REAL,report TEXT NOT NULL CHECK(json_valid(report)),calibrated_ms INTEGER NOT NULL,PRIMARY KEY(port,question));";

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
fn ledger_error(error: impl std::fmt::Display) -> DeciderError {
    DeciderError::Ledger(error.to_string())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    UserCorrection,
    TaskOutcome,
    Review,
}
impl LabelSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::UserCorrection => "user_correction",
            Self::TaskOutcome => "task_outcome",
            Self::Review => "review",
        }
    }
}
pub(crate) struct NewRecord {
    pub port: DecisionPort,
    pub question: String,
    pub kind: String,
    pub backend: String,
    pub probability: f64,
    pub verdict: Verdict,
    pub selected: Option<String>,
    pub input_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LedgerRecord {
    pub id: String,
    pub port: String,
    pub question: String,
    pub kind: String,
    pub backend: String,
    pub probability: f64,
    pub verdict: String,
    pub selected: Option<String>,
    pub input_hash: String,
    pub label: Option<bool>,
    pub label_source: Option<String>,
}

#[derive(Clone)]
pub struct DecisionLedger {
    db: Database,
    counter: std::sync::Arc<AtomicU64>,
}
impl DecisionLedger {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::open(path, DatabaseKind::Telemetry, StorageConfig::default())
            .map_err(ledger_error)?;
        Self::with_database(db).await
    }
    /// Reuse an already open telemetry database.
    pub async fn with_database(db: Database) -> Result<Self> {
        let checksum = digest(SCHEMA.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(SCHEMA)?;
            let old: Option<String> = c
                .query_row(
                    "SELECT checksum FROM decider_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if old.as_ref().is_some_and(|old| old != &checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "decider ledger checksum mismatch",
                ));
            }
            c.execute(
                "INSERT OR IGNORE INTO decider_schema VALUES(1,?1)",
                [&checksum],
            )?;
            Ok(())
        })
        .await
        .map_err(ledger_error)?;
        Ok(Self {
            db,
            counter: Default::default(),
        })
    }
    pub(crate) async fn record(&self, record: NewRecord) -> Result<String> {
        let sequence = self.counter.fetch_add(1, Ordering::Relaxed);
        let created = now_ms();
        let id = format!(
            "dec_{}",
            &digest(
                format!(
                    "{}:{}:{}:{created}:{sequence}:{}",
                    record.port.as_str(),
                    record.question,
                    record.input_hash,
                    std::process::id()
                )
                .as_bytes()
            )[..32]
        );
        let row = id.clone();
        let verdict = serde_json::to_value(record.verdict)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        self.db
            .write(Priority::Normal, move |c| {
                c.execute(
                    "INSERT INTO decider_records(id,port,question,kind,backend,probability,verdict,selected,input_hash,created_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![row, record.port.as_str(), record.question, record.kind, record.backend, record.probability, verdict, record.selected, record.input_hash, created],
                )?;
                Ok(())
            })
            .await
            .map_err(ledger_error)?;
        Ok(id)
    }
    /// Attach the observed outcome. A label is immutable once written.
    pub async fn label(&self, id: &str, outcome: bool, source: LabelSource) -> Result<()> {
        let id = id.to_string();
        let changed = self
            .db
            .write(Priority::Normal, move |c| {
                let current: Option<Option<i64>> = c
                    .query_row(
                        "SELECT label FROM decider_records WHERE id=?1",
                        [&id],
                        |r| r.get(0),
                    )
                    .optional()?;
                match current {
                    None => Err(excalibur_store::Error::Invalid("unknown decision record")),
                    Some(Some(old)) if (old == 1) != outcome => Err(excalibur_store::Error::Invalid(
                        "decision record already carries a different label",
                    )),
                    Some(Some(_)) => Ok(false),
                    Some(None) => {
                        c.execute(
                            "UPDATE decider_records SET label=?2,label_source=?3,labeled_ms=?4 WHERE id=?1 AND label IS NULL",
                            rusqlite::params![id, i64::from(outcome), source.as_str(), now_ms()],
                        )?;
                        Ok(true)
                    }
                }
            })
            .await
            .map_err(ledger_error)?;
        let _ = changed;
        Ok(())
    }
    pub async fn records(&self, port: DecisionPort) -> Result<Vec<LedgerRecord>> {
        let port = port.as_str().to_string();
        self.db
            .read(ReadLane::Background, move |c| {
                let mut q = c.prepare("SELECT id,port,question,kind,backend,probability,verdict,selected,input_hash,label,label_source FROM decider_records WHERE port=?1 ORDER BY created_ms,id")?;
                let rows = q
                    .query_map([&port], |r| {
                        Ok(LedgerRecord {
                            id: r.get(0)?,
                            port: r.get(1)?,
                            question: r.get(2)?,
                            kind: r.get(3)?,
                            backend: r.get(4)?,
                            probability: r.get(5)?,
                            verdict: r.get(6)?,
                            selected: r.get(7)?,
                            input_hash: r.get(8)?,
                            label: r.get::<_, Option<i64>>(9)?.map(|v| v == 1),
                            label_source: r.get(10)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(ledger_error)
    }
    pub async fn labeled(&self, port: DecisionPort, question: &str) -> Result<Vec<LabeledOutcome>> {
        let port = port.as_str().to_string();
        let question = question.to_string();
        self.db
            .read(ReadLane::Background, move |c| {
                let mut q = c.prepare("SELECT probability,label FROM decider_records WHERE port=?1 AND question=?2 AND label IS NOT NULL ORDER BY created_ms,id")?;
                let rows = q
                    .query_map([&port, &question], |r| {
                        Ok(LabeledOutcome {
                            probability: r.get(0)?,
                            outcome: r.get::<_, i64>(1)? == 1,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(ledger_error)
    }
    pub(crate) async fn store_thresholds(
        &self,
        port: DecisionPort,
        question: &str,
        report: &CalibrationReport,
    ) -> Result<()> {
        let port = port.as_str().to_string();
        let question = question.to_string();
        let body = serde_json::to_string(report).map_err(ledger_error)?;
        let (low, high) = (report.thresholds.tau_low, report.thresholds.tau_high);
        self.db
            .write(Priority::Normal, move |c| {
                c.execute(
                    "INSERT INTO decider_thresholds VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(port,question) DO UPDATE SET tau_low=excluded.tau_low,tau_high=excluded.tau_high,report=excluded.report,calibrated_ms=excluded.calibrated_ms",
                    rusqlite::params![port, question, low, high, body, now_ms()],
                )?;
                Ok(())
            })
            .await
            .map_err(ledger_error)
    }
    pub async fn calibration_report(
        &self,
        port: DecisionPort,
        question: &str,
    ) -> Result<Option<CalibrationReport>> {
        let port = port.as_str().to_string();
        let question = question.to_string();
        let body: Option<String> = self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row(
                    "SELECT report FROM decider_thresholds WHERE port=?1 AND question=?2",
                    [&port, &question],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .await
            .map_err(ledger_error)?;
        body.map(|b| serde_json::from_str(&b).map_err(ledger_error))
            .transpose()
    }
    pub(crate) async fn thresholds(&self) -> Result<BTreeMap<(DecisionPort, String), Thresholds>> {
        let rows: Vec<(String, String, Option<f64>, Option<f64>)> = self
            .db
            .read(ReadLane::Background, |c| {
                let mut q = c.prepare("SELECT port,question,tau_low,tau_high FROM decider_thresholds")?;
                let rows = q
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(ledger_error)?;
        let mut result = BTreeMap::new();
        for (port, question, tau_low, tau_high) in rows {
            let thresholds = Thresholds {
                tau_low,
                tau_high,
                calibrated: true,
            };
            thresholds.validate()?;
            result.insert((DecisionPort::parse(&port)?, question), thresholds);
        }
        Ok(result)
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.db.shutdown().await.map_err(ledger_error)
    }
}
