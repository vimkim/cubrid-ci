use std::fmt;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    #[serde(rename = "test_medium")]
    #[value(name = "test_medium")]
    Medium,
    #[serde(rename = "test_sql")]
    #[value(name = "test_sql")]
    Sql,
    #[serde(rename = "test_shell")]
    #[value(name = "test_shell")]
    Shell,
}

impl Suite {
    pub const ALL: [Self; 3] = [Self::Medium, Self::Sql, Self::Shell];

    pub const fn job_name(self) -> &'static str {
        match self {
            Self::Medium => "test_medium",
            Self::Sql => "test_sql",
            Self::Shell => "test_shell",
        }
    }
}

impl fmt::Display for Suite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.job_name())
    }
}
