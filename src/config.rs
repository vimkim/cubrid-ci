use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct ConfigOverride {
    pub data_dir: Option<PathBuf>,
    pub artifact_base: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub data_dir: PathBuf,
    pub data_dir_source: ConfigSource,
    pub artifact_base: Option<Url>,
    pub artifact_base_source: Option<ConfigSource>,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigSource {
    CommandLine,
    Environment,
    ConfigFile,
    Default,
}

impl ConfigSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CommandLine => "command_line",
            Self::Environment => "environment",
            Self::ConfigFile => "config_file",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    artifact_base: Option<String>,
}

impl ResolvedConfig {
    pub fn load(overrides: ConfigOverride) -> Result<Self, AppError> {
        let file = load_file_config()?;
        let environment_data_dir = nonempty_env("CUBRID_CI_DATA_DIR").map(PathBuf::from);
        let environment_artifact_base = nonempty_env("CUBRID_CI_ARTIFACT_BASE");

        let (data_dir, data_dir_source) = if let Some(path) = overrides.data_dir {
            (path, ConfigSource::CommandLine)
        } else if let Some(path) = environment_data_dir {
            (path, ConfigSource::Environment)
        } else if let Some(path) = file.data_dir {
            (path, ConfigSource::ConfigFile)
        } else {
            (default_data_dir()?, ConfigSource::Default)
        };

        let (artifact_base, artifact_base_source) = if let Some(value) = overrides.artifact_base {
            (
                Some(parse_artifact_base(&value)?),
                Some(ConfigSource::CommandLine),
            )
        } else if let Some(value) = environment_artifact_base {
            (
                Some(parse_artifact_base(&value.to_string_lossy())?),
                Some(ConfigSource::Environment),
            )
        } else if let Some(value) = file.artifact_base {
            (
                Some(parse_artifact_base(&value)?),
                Some(ConfigSource::ConfigFile),
            )
        } else {
            (None, None)
        };

        Ok(Self {
            data_dir,
            data_dir_source,
            artifact_base,
            artifact_base_source,
        })
    }
}

fn load_file_config() -> Result<FileConfig, AppError> {
    let path = config_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileConfig::default());
        }
        Err(error) => return Err(AppError::storage(path, error)),
    };
    toml::from_str(&contents)
        .map_err(|error| AppError::Input(format!("invalid config {}: {error}", path.display())))
}

fn config_path() -> Result<PathBuf, AppError> {
    if let Some(path) = nonempty_env("CUBRID_CI_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(root) = nonempty_env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("cubrid-ci/config.toml"));
    }
    Ok(user_home()?.join(".config/cubrid-ci/config.toml"))
}

fn default_data_dir() -> Result<PathBuf, AppError> {
    if let Some(root) = nonempty_env("XDG_DATA_HOME") {
        return Ok(PathBuf::from(root).join("cubrid-ci-data"));
    }
    Ok(user_home()?.join(".local/share/cubrid-ci-data"))
}

fn user_home() -> Result<PathBuf, AppError> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Input("HOME is not set; configure CUBRID_CI_DATA_DIR".to_owned()))
}

fn nonempty_env(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

fn parse_artifact_base(value: &str) -> Result<Url, AppError> {
    let mut url = Url::parse(value)
        .map_err(|error| AppError::Input(format!("invalid artifact base URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        return Err(AppError::Input(
            "artifact base URL must be an http or https base URL".to_owned(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::Input(
            "artifact base URL must not contain credentials, a query, or a fragment".to_owned(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

pub fn nearest_existing_path(path: &Path) -> Option<&Path> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if current.exists() {
            return Some(current);
        }
        candidate = current.parent();
    }
    None
}
