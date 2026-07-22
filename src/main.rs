use std::process::ExitCode;

use clap::Parser;
use cubrid_circleci_analyzer::{Cli, Collector, CollectorConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let config = match CollectorConfig::from_cli(&cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(error.exit_code());
        }
    };

    match Collector::new(config) {
        Ok(collector) => match collector.run().await {
            Ok(result) => {
                if cli.json {
                    match serde_json::to_string_pretty(&result) {
                        Ok(json) => println!("{json}"),
                        Err(error) => {
                            eprintln!("error: failed to serialize command result: {error}");
                            return ExitCode::from(6);
                        }
                    }
                } else {
                    println!("{}", result.human_summary());
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                if cli.json {
                    let json = serde_json::json!({
                        "ok": false,
                        "kind": error.kind().as_str(),
                        "error": error.to_string(),
                        "exit_code": error.exit_code(),
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json).unwrap_or_default()
                    );
                } else {
                    eprintln!("error: {error}");
                }
                ExitCode::from(error.exit_code())
            }
        },
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn init_tracing(verbose: u8) {
    let fallback = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
