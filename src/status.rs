use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::cli::StatusArgs;
use crate::error::AppError;

pub fn execute(args: &StatusArgs, json: bool) -> AppError {
    let mut command = Command::new("cubrid-pr-status");
    command.args(delegated_args(args, json));
    let error = command.exec();
    AppError::Input(format!(
        "failed to execute cubrid-pr-status; install it and ensure it is on PATH: {error}"
    ))
}

fn delegated_args(args: &StatusArgs, json: bool) -> Vec<OsString> {
    let mut delegated = Vec::new();
    if json {
        delegated.push("--json".into());
    }
    if args.human {
        delegated.push("--human".into());
    }
    if let Some(history) = args.history {
        delegated.push("--history".into());
        delegated.push(history.to_string().into());
    }
    if let Some(config) = &args.config {
        delegated.push("--config".into());
        delegated.push(config.as_os_str().to_owned());
    }
    if args.watch {
        delegated.push("--watch".into());
    }
    if let Some(interval) = args.interval {
        delegated.push("--interval".into());
        delegated.push(interval.to_string().into());
    }
    if let Some(pr) = &args.pr {
        delegated.push(canonical_pr(pr).into());
    }
    delegated
}

fn canonical_pr(pr: &str) -> String {
    if pr.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("https://github.com/CUBRID/cubrid/pull/{pr}")
    } else {
        pr.to_owned()
    }
}
