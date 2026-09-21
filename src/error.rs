use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Input,
    Unavailable,
    Integrity,
    Remote,
    Storage,
}

impl ExitKind {
    pub const fn code(self) -> u8 {
        match self {
            Self::Input => 2,
            Self::Unavailable => 3,
            Self::Integrity => 4,
            Self::Remote => 5,
            Self::Storage => 6,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Unavailable => "unavailable",
            Self::Integrity => "integrity",
            Self::Remote => "remote",
            Self::Storage => "storage",
        }
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("invalid input: {0}")]
    Input(String),

    #[error("current result unavailable: {0}")]
    Unavailable(String),

    #[error("evidence expired: {0}")]
    Expired(String),

    #[error("evidence exceeds configured limit: {0}")]
    Oversized(String),

    #[error("malformed evidence: {0}")]
    Malformed(String),

    #[error("CI pipeline failed before trustworthy testcase evidence: {0}")]
    JobLevel(String),

    #[error("result identity mismatch: {0}")]
    Integrity(String),

    #[error("remote request failed: {0}")]
    Remote(String),

    #[error("storage operation failed for {path}: {source}")]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid remote JSON from {endpoint}: {source}")]
    Json {
        endpoint: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl AppError {
    pub const fn kind(&self) -> ExitKind {
        match self {
            Self::Input(_) => ExitKind::Input,
            Self::Unavailable(_) | Self::Expired(_) | Self::JobLevel(_) => ExitKind::Unavailable,
            Self::Integrity(_) | Self::Malformed(_) => ExitKind::Integrity,
            Self::Remote(_) | Self::Oversized(_) | Self::Json { .. } => ExitKind::Remote,
            Self::Storage { .. } | Self::Serialization(_) => ExitKind::Storage,
        }
    }

    pub const fn diagnostic_kind(&self) -> &'static str {
        match self {
            Self::Input(_) => "input",
            Self::Unavailable(_) => "unavailable",
            Self::Expired(_) => "expired",
            Self::Oversized(_) => "oversized",
            Self::Malformed(_) => "malformed",
            Self::JobLevel(_) => "job_level",
            Self::Integrity(_) => "identity_or_integrity",
            Self::Remote(_) => "remote",
            Self::Json { .. } => "malformed",
            Self::Storage { .. } | Self::Serialization(_) => "storage",
        }
    }

    pub const fn exit_code(&self) -> u8 {
        self.kind().code()
    }

    pub fn storage(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Storage {
            path: path.into(),
            source,
        }
    }
}
