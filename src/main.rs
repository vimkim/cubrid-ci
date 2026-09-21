use std::process::ExitCode;

use clap::Parser;
use cubrid_circleci_analyzer::cli::Commands;
use cubrid_circleci_analyzer::config::{ConfigOverride, ResolvedConfig};
use cubrid_circleci_analyzer::doctor::DoctorResult;
use cubrid_circleci_analyzer::{Cli, Collector, CollectorConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Commands::Status(args) = &cli.command {
        let error = cubrid_circleci_analyzer::status::execute(args, cli.json);
        emit_error(&cli, &error);
        return ExitCode::from(error.exit_code());
    }

    if let Commands::Doctor(args) = &cli.command {
        let config = match ResolvedConfig::load(ConfigOverride {
            data_dir: args.data_dir.clone(),
            artifact_base: args.artifact_base.clone(),
        }) {
            Ok(config) => config,
            Err(error) => {
                emit_error(&cli, &error);
                return ExitCode::from(error.exit_code());
            }
        };
        let result = DoctorResult::run(config).await;
        if cli.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_owned())
            );
        } else {
            println!("{}", result.human_summary());
        }
        return if result.ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        };
    }

    let config = match CollectorConfig::from_cli(&cli) {
        Ok(config) => config,
        Err(error) => {
            emit_error(&cli, &error);
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
                emit_error(&cli, &error);
                ExitCode::from(error.exit_code())
            }
        },
        Err(error) => {
            emit_error(&cli, &error);
            ExitCode::from(error.exit_code())
        }
    }
}

fn emit_error(cli: &Cli, error: &cubrid_circleci_analyzer::AppError) {
    if cli.json {
        let json = serde_json::json!({
            "ok": false,
            "kind": error.kind().as_str(),
            "error": error.to_string(),
            "exit_code": error.exit_code(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        eprintln!("error: {error}");
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
