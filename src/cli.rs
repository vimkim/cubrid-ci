use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand};

use crate::model::Suite;

#[derive(Debug, Parser)]
#[command(
    name = "cubrid-ci",
    version = crate::build_info::VERSION,
    about = "Inspect and collect exact-commit CUBRID CI evidence",
    propagate_version = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Emit one machine-readable JSON value to stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase diagnostic logging (-v for info, -vv for debug).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Show a pull-request status snapshot using cubrid-pr-status.
    Status(StatusArgs),
    /// Collect exact-commit GitHub Actions evidence.
    Collect(CollectArgs),
    /// Check dependencies and local collection configuration.
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, Args)]
pub struct CollectArgs {
    /// CUBRID pull-request number or URL; otherwise detect from the current directory.
    pub pr: Option<String>,

    /// Exact 40-character commit SHA; required when PR is explicit.
    #[arg(long)]
    pub commit: Option<String>,

    /// Suite to collect; repeat to select a subset (all suites by default).
    #[arg(long, value_enum)]
    pub suite: Vec<Suite>,

    /// Root directory for durable evidence.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Internal CI evidence-server base URL.
    #[arg(long)]
    pub artifact_base: Option<String>,

    /// Poll the selected commit until every requested suite is terminal.
    #[arg(long)]
    pub wait: bool,

    /// Maximum time to wait for requested suites.
    #[arg(long, default_value = "26h", value_parser = parse_duration)]
    pub timeout: Duration,

    /// Delay between status snapshot polls while waiting.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub poll_interval: Duration,

    /// Download bounded artifacts from abnormal shards.
    #[arg(long)]
    pub include_binaries: bool,

    /// Maximum bytes downloaded for one abnormal-shard artifact.
    #[arg(long, default_value_t = 268_435_456)]
    pub max_binary_bytes: u64,

    /// Maximum artifact bytes downloaded across the collection.
    #[arg(long, default_value_t = 536_870_912)]
    pub max_binary_total_bytes: u64,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// Root directory for durable evidence.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Internal CI evidence-server base URL.
    #[arg(long)]
    pub artifact_base: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// CUBRID pull-request number or URL; otherwise detect from the current directory.
    pub pr: Option<String>,

    /// Number of previous PR commits to search.
    #[arg(long)]
    pub history: Option<u16>,

    /// Expected-check configuration passed to cubrid-pr-status.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Refresh until interrupted.
    #[arg(long)]
    pub watch: bool,

    /// Seconds between watch refreshes.
    #[arg(long)]
    pub interval: Option<u64>,

    /// Force human-readable output.
    #[arg(long, conflicts_with = "json")]
    pub human: bool,
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn exposes_only_the_three_public_commands() {
        for command in ["status", "collect", "doctor"] {
            assert!(Cli::try_parse_from(["cubrid-ci", command, "--help"]).is_err());
        }
        for removed in ["test-medium", "test-sql", "test-shell"] {
            assert!(Cli::try_parse_from(["cubrid-ci", removed]).is_err());
        }
    }
}
