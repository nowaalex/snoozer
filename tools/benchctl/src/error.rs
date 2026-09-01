use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum BenchError {
    #[error("invalid request: {0}")]
    Usage(String),
    #[error("preflight rejected the operation: {0}")]
    Preflight(String),
    #[error("operation {operation_id} conflicts with its recorded request")]
    RequestConflict { operation_id: String },
    #[error("operation {operation_id} needs explicit recovery")]
    RecoveryRequired { operation_id: String },
    #[error("cpuidle restore conflict at {path}: expected current {expected}, found {actual}")]
    RestoreConflict {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("operation state is unavailable: {0}")]
    State(String),
    #[error("workload failed: {0}")]
    Workload(String),
    #[error("I/O during {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("JSON during {operation}: {source}")]
    Json {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl BenchError {
    pub(crate) fn retry_advice(&self) -> &'static str {
        match self {
            Self::RequestConflict { .. } | Self::Usage(_) | Self::Preflight(_) => {
                "do not retry unchanged"
            }
            Self::RecoveryRequired { .. } | Self::RestoreConflict { .. } => {
                "run `benchctl recover <id>`"
            }
            Self::Workload(_) => {
                "inspect workload output; a new operation is safe only after successful cleanup"
            }
            Self::State(_) | Self::Io { .. } | Self::Json { .. } => {
                "inspect state and retry only when the cause is resolved"
            }
        }
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Workload(_) => ExitCode::from(1),
            _ => ExitCode::from(2),
        }
    }

    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}
