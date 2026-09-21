use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::model::Suite;

#[derive(Debug, Parser)]
#[command(
    name = "cubrid-ci",
    version = crate::build_info::VERSION,
    about = "Fetch exact-commit CUBRID CircleCI failure evidence",
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

    #[arg(
        long,
        global = true,
        env = "CUBRID_CI_GITHUB_API",
        default_value = "https://api.github.com",
        hide = true
    )]
    pub github_api: String,

    #[arg(
        long,
        global = true,
        env = "CUBRID_CI_CIRCLECI_API",
        default_value = "https://circleci.com/api/v1.1",
        hide = true
    )]
    pub circleci_api: String,
}

impl Cli {
    pub fn suite_and_args(&self) -> Option<(Suite, &FetchArgs)> {
        match &self.command {
            Commands::Status(_) | Commands::Doctor(_) => None,
            Commands::TestMedium(args) => Some((Suite::Medium, args)),
            Commands::TestSql(args) => Some((Suite::Sql, args)),
            Commands::TestShell(args) => Some((Suite::Shell, args)),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Show a pull-request status snapshot using cubrid-pr-status.
    Status(StatusArgs),
    /// Check dependencies and local collection configuration.
    Doctor(DoctorArgs),
    /// Fetch the test_medium result.
    TestMedium(FetchArgs),
    /// Fetch the test_sql result.
    TestSql(FetchArgs),
    /// Fetch the test_shell result.
    TestShell(FetchArgs),
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

#[derive(Debug, Clone, Args)]
pub struct FetchArgs {
    /// CUBRID GitHub pull-request URL.
    pub github_pr_url: String,

    /// Full or abbreviated commit SHA. Defaults to the PR head at command start.
    pub commit: Option<String>,

    /// Root directory for collected evidence.
    #[arg(long, default_value = "data")]
    pub data_dir: PathBuf,

    /// Wait for build prerequisites and the selected suite to become terminal.
    #[arg(long)]
    pub wait: bool,

    /// Maximum wait duration.
    #[arg(long, default_value = "26h", value_parser = parse_duration)]
    pub timeout: Duration,

    /// Delay between GitHub status polls.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub poll_interval: Duration,

    /// Fetch one specific CircleCI rerun, after verifying its commit and job name.
    #[arg(long)]
    pub attempt: Option<u64>,

    /// Artifact download policy.
    #[arg(long, value_enum, default_value_t = ArtifactMode::Manifest)]
    pub artifact_mode: ArtifactMode,

    /// Maximum downloaded bytes per log or artifact.
    #[arg(long, default_value_t = 268_435_456)]
    pub max_artifact_bytes: u64,

    /// Maximum number of concurrent artifact downloads.
    #[arg(long, default_value_t = 4)]
    pub download_concurrency: usize,

    /// Download testcase and answer links found in failure messages when accessible.
    #[arg(long)]
    pub include_test_sources: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMode {
    /// Record artifact metadata but download none.
    #[default]
    Manifest,
    /// Download bounded textual diagnostics and XML.
    Text,
    /// Download every bounded artifact, including core dumps.
    All,
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_three_subcommands() {
        for (name, suite) in [
            ("test-medium", Suite::Medium),
            ("test-sql", Suite::Sql),
            ("test-shell", Suite::Shell),
        ] {
            let cli = Cli::try_parse_from([
                "cubrid-ci",
                name,
                "https://github.com/CUBRID/cubrid/pull/6864",
                "c2cbeaf",
                "--artifact-mode",
                "manifest",
            ])
            .unwrap();
            assert_eq!(cli.suite_and_args().unwrap().0, suite);
            assert_eq!(
                cli.suite_and_args().unwrap().1.commit.as_deref(),
                Some("c2cbeaf")
            );
        }
    }

    #[test]
    fn defaults_to_manifest_artifact_mode() {
        let cli = Cli::try_parse_from([
            "cubrid-ci",
            "test-shell",
            "https://github.com/CUBRID/cubrid/pull/6864",
        ])
        .unwrap();

        assert_eq!(
            cli.suite_and_args().unwrap().1.artifact_mode,
            ArtifactMode::Manifest
        );
    }

    #[test]
    fn accepts_explicit_artifact_download_modes() {
        for (value, expected) in [("text", ArtifactMode::Text), ("all", ArtifactMode::All)] {
            let cli = Cli::try_parse_from([
                "cubrid-ci",
                "test-shell",
                "https://github.com/CUBRID/cubrid/pull/6864",
                "--artifact-mode",
                value,
            ])
            .unwrap();

            assert_eq!(cli.suite_and_args().unwrap().1.artifact_mode, expected);
        }
    }
}
