use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;

use crate::config::{ResolvedConfig, nearest_existing_path};

#[derive(Debug, Serialize)]
pub struct DoctorResult {
    pub ok: bool,
    pub data_dir: String,
    pub data_dir_source: &'static str,
    pub artifact_base: Option<String>,
    pub artifact_base_source: Option<&'static str>,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub state: CheckState,
    pub detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Healthy,
    Warning,
    Deferred,
    Failing,
}

impl DoctorResult {
    pub async fn run(config: ResolvedConfig) -> Self {
        let mut checks = vec![command_check(
            "cubrid-pr-status",
            "cubrid-pr-status",
            &["--help"],
        )];
        checks.push(command_check("gh", "gh", &["--version"]));
        checks.push(command_check(
            "gh-auth",
            "gh",
            &["auth", "status", "--hostname", "github.com"],
        ));
        checks.push(evidence_root_check(&config));
        checks.push(evidence_server_check(&config).await);
        let ok = checks
            .iter()
            .all(|check| !matches!(check.state, CheckState::Failing));
        Self {
            ok,
            data_dir: config.data_dir.display().to_string(),
            data_dir_source: config.data_dir_source.as_str(),
            artifact_base: config.artifact_base.as_ref().map(ToString::to_string),
            artifact_base_source: config.artifact_base_source.map(|source| source.as_str()),
            checks,
        }
    }

    pub fn human_summary(&self) -> String {
        let mut lines = vec![format!(
            "cubrid-ci doctor: {}",
            if self.ok { "healthy" } else { "failing" }
        )];
        lines.push(format!(
            "evidence root: {} ({})",
            self.data_dir, self.data_dir_source
        ));
        for check in &self.checks {
            lines.push(format!(
                "- {}: {} — {}",
                check.name,
                check.state.as_str(),
                check.detail
            ));
        }
        lines.join("\n")
    }
}

impl CheckState {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Warning => "warning",
            Self::Deferred => "deferred",
            Self::Failing => "failing",
        }
    }
}

fn command_check(name: &'static str, command: &str, args: &[&str]) -> DoctorCheck {
    let result = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(status) if status.success() => DoctorCheck {
            name,
            state: CheckState::Healthy,
            detail: "available".to_owned(),
        },
        Ok(status) => DoctorCheck {
            name,
            state: CheckState::Failing,
            detail: format!("exited with {status}"),
        },
        Err(error) => DoctorCheck {
            name,
            state: CheckState::Failing,
            detail: error.to_string(),
        },
    }
}

fn evidence_root_check(config: &ResolvedConfig) -> DoctorCheck {
    let Some(existing_path) = nearest_existing_path(&config.data_dir) else {
        return DoctorCheck {
            name: "evidence-root",
            state: CheckState::Failing,
            detail: "no existing parent directory".to_owned(),
        };
    };
    if !existing_path.is_dir() {
        return DoctorCheck {
            name: "evidence-root",
            state: CheckState::Failing,
            detail: format!(
                "{} blocks creation because it is not a directory",
                existing_path.display()
            ),
        };
    }
    let root_exists = config.data_dir.exists();
    let probe_dir = existing_path;
    match tempfile::NamedTempFile::new_in(probe_dir) {
        Ok(_) => DoctorCheck {
            name: "evidence-root",
            state: if root_exists {
                CheckState::Healthy
            } else {
                CheckState::Warning
            },
            detail: if root_exists {
                format!("writable at {}", probe_dir.display())
            } else {
                format!(
                    "not created yet; writable parent is {}",
                    probe_dir.display()
                )
            },
        },
        Err(error) => DoctorCheck {
            name: "evidence-root",
            state: CheckState::Failing,
            detail: error.to_string(),
        },
    }
}

async fn evidence_server_check(config: &ResolvedConfig) -> DoctorCheck {
    let Some(url) = &config.artifact_base else {
        return DoctorCheck {
            name: "evidence-server",
            state: CheckState::Deferred,
            detail: "automatic discovery occurs during collection".to_owned(),
        };
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return DoctorCheck {
                name: "evidence-server",
                state: CheckState::Failing,
                detail: error.to_string(),
            };
        }
    };
    match client.get(url.clone()).send().await {
        Ok(response) if response.status().is_success() => DoctorCheck {
            name: "evidence-server",
            state: CheckState::Healthy,
            detail: "reachable".to_owned(),
        },
        Ok(response) => DoctorCheck {
            name: "evidence-server",
            state: CheckState::Failing,
            detail: format!("returned {}", response.status()),
        },
        Err(error) => DoctorCheck {
            name: "evidence-server",
            state: CheckState::Failing,
            detail: error.to_string(),
        },
    }
}
