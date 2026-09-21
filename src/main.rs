use std::process::ExitCode;

use clap::Parser;
use cubrid_ci::Cli;
use cubrid_ci::cli::Commands;
use cubrid_ci::config::{ConfigOverride, ResolvedConfig};
use cubrid_ci::doctor::DoctorResult;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Commands::Status(args) = &cli.command {
        let error = cubrid_ci::status::execute(args, cli.json);
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

    if let Commands::Collect(args) = &cli.command {
        return match cubrid_ci::gha_collect::run(args).await {
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
                ExitCode::from(result.exit_code())
            }
            Err(error) => {
                emit_error(&cli, &error);
                ExitCode::from(error.exit_code())
            }
        };
    }

    unreachable!("clap requires one of status, collect, or doctor")
}

fn emit_error(cli: &Cli, error: &cubrid_ci::AppError) {
    if cli.json {
        let json = serde_json::json!({
            "ok": false,
            "kind": error.kind().as_str(),
            "diagnostic": error.diagnostic_kind(),
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
