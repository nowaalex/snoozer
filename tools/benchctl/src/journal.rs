use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::BenchError;
use crate::runtime::atomic_write;

pub(crate) const JOURNAL_VERSION: &str = "benchctl-journal-v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Actor {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Request {
    pub(crate) receipt: PathBuf,
    pub(crate) accepted_receipt: PathBuf,
    pub(crate) receipt_digest: String,
    pub(crate) accepted_executable: PathBuf,
    pub(crate) executable_digest: String,
    pub(crate) cpus: Vec<usize>,
    pub(crate) timeout_seconds: u64,
    pub(crate) workload: Vec<String>,
    pub(crate) workload_actor: Actor,
}

impl Request {
    pub(crate) fn digest(&self) -> Result<String, BenchError> {
        let encoded = serde_json::to_vec(self).map_err(|source| BenchError::Json {
            operation: "hashing request",
            source,
        })?;
        Ok(hex_digest(&encoded))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct IdleEntry {
    pub(crate) cpu: usize,
    pub(crate) state: usize,
    pub(crate) name: String,
    pub(crate) disable_path: PathBuf,
    pub(crate) original: String,
    pub(crate) desired: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Prepared,
    Applied,
    WorkloadStarted,
    WorkloadFinished,
    Draining,
    Restoring,
    Restored,
    Recoverable,
    RestoreConflict,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub(crate) enum TerminalOutcome {
    Success,
    Failed(String),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Transition {
    pub(crate) stage: Stage,
    pub(crate) coordinator: Actor,
    pub(crate) unix_millis: u128,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Journal {
    pub(crate) version: String,
    pub(crate) operation_id: String,
    pub(crate) request: Request,
    pub(crate) request_hash: String,
    pub(crate) coordinator: Actor,
    pub(crate) boot_id: String,
    pub(crate) entries: Vec<IdleEntry>,
    #[serde(default)]
    pub(crate) outcome: Option<TerminalOutcome>,
    pub(crate) transitions: Vec<Transition>,
}

impl Journal {
    pub(crate) fn new(
        operation_id: String,
        request: Request,
        coordinator: Actor,
        boot_id: String,
    ) -> Result<Self, BenchError> {
        let request_hash = request.digest()?;
        Ok(Self {
            version: JOURNAL_VERSION.to_owned(),
            operation_id,
            request,
            request_hash,
            coordinator: coordinator.clone(),
            boot_id,
            entries: Vec::new(),
            outcome: None,
            transitions: vec![Transition {
                stage: Stage::Prepared,
                coordinator,
                unix_millis: now_millis(),
            }],
        })
    }

    pub(crate) fn stage(&self) -> Stage {
        self.transitions
            .last()
            .map(|value| value.stage)
            .unwrap_or(Stage::Prepared)
    }

    pub(crate) fn transition(&mut self, stage: Stage, actor: Actor) {
        self.transitions.push(Transition {
            stage,
            coordinator: actor,
            unix_millis: now_millis(),
        });
    }

    pub(crate) fn validate(&self) -> Result<(), BenchError> {
        if self.version != JOURNAL_VERSION {
            return Err(BenchError::State(format!(
                "unsupported journal version: {}",
                self.version
            )));
        }
        if self.request_hash != self.request.digest()? {
            return Err(BenchError::State(
                "journal request hash does not match its request".to_owned(),
            ));
        }
        if self.transitions.is_empty() {
            return Err(BenchError::State("journal has no transition".to_owned()));
        }
        Ok(())
    }
}

pub(crate) fn journal_path(state_root: &Path, operation_id: &str) -> Result<PathBuf, BenchError> {
    if operation_id.is_empty()
        || !operation_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'-' || value == b'_')
    {
        return Err(BenchError::Usage(
            "operation id must contain only ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }
    Ok(state_root.join(format!("{operation_id}.json")))
}

pub(crate) fn load(path: &Path) -> Result<Journal, BenchError> {
    let bytes = fs::read(path).map_err(|error| BenchError::io("reading journal", error))?;
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|source| BenchError::Json {
        operation: "reading journal",
        source,
    })?;
    journal.validate()?;
    Ok(journal)
}

pub(crate) fn store(path: &Path, journal: &Journal) -> Result<(), BenchError> {
    journal.validate()?;
    let bytes = serde_json::to_vec_pretty(journal).map_err(|source| BenchError::Json {
        operation: "serializing journal",
        source,
    })?;
    atomic_write(path, &bytes)
}

pub(crate) fn actor() -> Actor {
    Actor {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    }
}

pub(crate) fn boot_id() -> Result<String, BenchError> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|value| value.trim().to_owned())
        .map_err(|error| BenchError::io("reading boot identity", error))
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis())
}
