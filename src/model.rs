use std::fmt;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ValueEnum,
)]
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
    #[serde(rename = "test_sql_by_cci")]
    #[value(name = "test_sql_by_cci")]
    SqlByCci,
}

impl Suite {
    pub const ALL: [Self; 4] = [Self::Medium, Self::Sql, Self::Shell, Self::SqlByCci];

    /// Suites collected when no `--suite` is given. `test_sql_by_cci` is opt-in
    /// because develop commits carry no status for it, so a default request
    /// would leave every merge-base baseline incomplete.
    pub const DEFAULT: [Self; 3] = [Self::Medium, Self::Sql, Self::Shell];

    /// The suites a `--suite` list requests, in canonical order.
    pub fn requested(filter: &[Self]) -> Vec<Self> {
        if filter.is_empty() {
            Self::DEFAULT.to_vec()
        } else {
            Self::ALL
                .into_iter()
                .filter(|suite| filter.contains(suite))
                .collect()
        }
    }

    pub const fn job_name(self) -> &'static str {
        match self {
            Self::Medium => "test_medium",
            Self::Sql => "test_sql",
            Self::Shell => "test_shell",
            Self::SqlByCci => "test_sql_by_cci",
        }
    }
}

impl fmt::Display for Suite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.job_name())
    }
}
