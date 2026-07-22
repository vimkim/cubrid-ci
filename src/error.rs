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
            Self::Unavailable(_) => ExitKind::Unavailable,
            Self::Integrity(_) => ExitKind::Integrity,
            Self::Remote(_) | Self::Json { .. } => ExitKind::Remote,
            Self::Storage { .. } | Self::Serialization(_) => ExitKind::Storage,
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
