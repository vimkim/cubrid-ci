use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use tempfile::{Builder, NamedTempFile, TempDir};
use walkdir::WalkDir;

use crate::error::AppError;
use crate::model::{CommitManifest, Suite};

#[derive(Debug, Clone)]
pub struct Storage {
    data_root: PathBuf,
    directory_identity: String,
    short_sha: String,
    commit_dir: PathBuf,
}

impl Storage {
    pub fn new(
        data_root: impl Into<PathBuf>,
        directory_identity: impl Into<String>,
        short_sha: impl Into<String>,
    ) -> Self {
        let data_root = data_root.into();
        let directory_identity = directory_identity.into();
        let short_sha = short_sha.into();
        let commit_dir = data_root.join(&directory_identity).join(&short_sha);
        Self {
            data_root,
            directory_identity,
            short_sha,
            commit_dir,
        }
    }

    pub fn initialize(&self) -> Result<(), AppError> {
        create_dir_all(&self.commit_dir)?;
        for suite in Suite::ALL {
            create_dir_all(&self.suite_dir(suite))?;
        }
        Ok(())
    }

    pub fn commit_dir(&self) -> &Path {
        &self.commit_dir
    }

    pub fn suite_dir(&self, suite: Suite) -> PathBuf {
        self.commit_dir.join(suite.job_name())
    }

    pub fn write_manifest(&self, manifest: &CommitManifest) -> Result<(), AppError> {
        write_json_atomic(&self.commit_dir.join("manifest.json"), manifest)
    }

    pub fn staging(&self, suite: Suite) -> Result<TempDir, AppError> {
        Builder::new()
            .prefix(&format!(".{}.staging-", suite.job_name()))
            .tempdir_in(&self.commit_dir)
            .map_err(|source| AppError::storage(&self.commit_dir, source))
    }

    pub fn seed_attempt_history(&self, suite: Suite, staging: &Path) -> Result<(), AppError> {
        let source = self.suite_dir(suite).join("attempts");
        if source.exists() {
            clone_tree(&source, &staging.join("attempts"))?;
        }
        Ok(())
    }

    pub fn publish_suite(&self, suite: Suite, staging: TempDir) -> Result<PathBuf, AppError> {
        let destination = self.suite_dir(suite);
        let backup = self
            .commit_dir
            .join(format!(".{}.backup", suite.job_name()));
        if backup.exists() {
            fs::remove_dir_all(&backup).map_err(|source| AppError::storage(&backup, source))?;
        }

        let staged_path = staging.keep();
        if destination.exists() {
            fs::rename(&destination, &backup)
                .map_err(|source| AppError::storage(&destination, source))?;
        }
        match fs::rename(&staged_path, &destination) {
            Ok(()) => {
                if backup.exists() {
                    fs::remove_dir_all(&backup)
                        .map_err(|source| AppError::storage(&backup, source))?;
                }
                sync_parent(&destination)?;
                Ok(destination)
            }
            Err(source) => {
                if backup.exists() {
                    let _ = fs::rename(&backup, &destination);
                }
                Err(AppError::storage(&staged_path, source))
            }
        }
    }

    pub fn identity(&self) -> (&str, &str) {
        (&self.directory_identity, &self.short_sha)
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }
}

pub fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &bytes)
}

pub fn write_string_atomic(path: &Path, value: &str) -> Result<(), AppError> {
    write_bytes_atomic(path, value.as_bytes())
}

pub fn write_bytes_atomic(path: &Path, value: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Input(format!("output path has no parent: {}", path.display())))?;
    create_dir_all(parent)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| AppError::storage(parent, source))?;
    temporary
        .write_all(value)
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    temporary
        .persist(path)
        .map_err(|error| AppError::storage(path, error.error))?;
    sync_parent(path)
}

pub fn write_bytes_immutable(path: &Path, value: &[u8]) -> Result<(), AppError> {
    match fs::read(path) {
        Ok(existing) if existing == value => return Ok(()),
        Ok(_) => {
            return Err(AppError::Integrity(format!(
                "immutable evidence already exists with different content: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(AppError::storage(path, error)),
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Input(format!("output path has no parent: {}", path.display())))?;
    create_dir_all(parent)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| AppError::storage(parent, source))?;
    temporary
        .write_all(value)
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    match temporary.persist_noclobber(path) {
        Ok(_) => sync_parent(path),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(path).map_err(|source| AppError::storage(path, source))?;
            if existing == value {
                Ok(())
            } else {
                Err(AppError::Integrity(format!(
                    "immutable evidence already exists with different content: {}",
                    path.display()
                )))
            }
        }
        Err(error) => Err(AppError::storage(path, error.error)),
    }
}

pub fn write_string_immutable(path: &Path, value: &str) -> Result<(), AppError> {
    write_bytes_immutable(path, value.as_bytes())
}

pub fn write_json_immutable(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_immutable(path, &bytes)
}

pub fn write_string_if_absent(path: &Path, value: &str) -> Result<(), AppError> {
    write_bytes_if_absent(path, value.as_bytes())
}

pub fn write_json_if_absent(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_if_absent(path, &bytes)
}

fn write_bytes_if_absent(path: &Path, value: &[u8]) -> Result<(), AppError> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Input(format!("output path has no parent: {}", path.display())))?;
    create_dir_all(parent)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| AppError::storage(parent, source))?;
    temporary
        .write_all(value)
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    match temporary.persist_noclobber(path) {
        Ok(_) => sync_parent(path),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(AppError::storage(path, error.error)),
    }
}

pub fn safe_relative_path(input: &str) -> PathBuf {
    let mut safe = PathBuf::new();
    for component in Path::new(input).components() {
        if let Component::Normal(value) = component {
            let sanitized = sanitize_component(value);
            if !sanitized.is_empty() && sanitized != OsStr::new(".") {
                safe.push(sanitized);
            }
        }
    }
    if safe.as_os_str().is_empty() {
        safe.push("artifact");
    }
    safe
}

fn sanitize_component(input: &OsStr) -> std::ffi::OsString {
    let value = input.to_string_lossy();
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    std::ffi::OsString::from(sanitized)
}

fn clone_tree(source: &Path, destination: &Path) -> Result<(), AppError> {
    create_dir_all(destination)?;
    for entry in WalkDir::new(source) {
        let entry = entry.map_err(|error| {
            AppError::Remote(format!("failed to enumerate {}: {error}", source.display()))
        })?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| AppError::Input(format!("failed to derive attempt path: {error}")))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                create_dir_all(parent)?;
            }
            if fs::hard_link(entry.path(), &target).is_err() {
                fs::copy(entry.path(), &target)
                    .map_err(|source| AppError::storage(&target, source))?;
            }
        }
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(|source| AppError::storage(path, source))
}

fn sync_parent(path: &Path) -> Result<(), AppError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let directory = fs::File::open(parent).map_err(|source| AppError::storage(parent, source))?;
    directory
        .sync_all()
        .map_err(|source| AppError::storage(parent, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_paths_cannot_escape_artifact_root() {
        assert_eq!(
            safe_relative_path("../../tmp/logs/a:b.log"),
            PathBuf::from("tmp/logs/a_b.log")
        );
        assert_eq!(safe_relative_path("/"), PathBuf::from("artifact"));
    }

    #[test]
    fn unavailable_layout_has_empty_suite_directories() {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage::new(root.path(), "CBRD-1", "abcdef0");
        storage.initialize().unwrap();
        for suite in Suite::ALL {
            assert_eq!(fs::read_dir(storage.suite_dir(suite)).unwrap().count(), 0);
        }
    }

    #[test]
    fn publishes_staging_atomically_and_preserves_attempts() {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage::new(root.path(), "CBRD-1", "abcdef0");
        storage.initialize().unwrap();

        let first = storage.staging(Suite::Sql).unwrap();
        write_string_atomic(&first.path().join("attempts/1/raw/job.json"), "{}").unwrap();
        write_string_atomic(&first.path().join("summary.json"), "one").unwrap();
        storage.publish_suite(Suite::Sql, first).unwrap();

        let second = storage.staging(Suite::Sql).unwrap();
        storage
            .seed_attempt_history(Suite::Sql, second.path())
            .unwrap();
        write_string_atomic(&second.path().join("summary.json"), "two").unwrap();
        storage.publish_suite(Suite::Sql, second).unwrap();

        assert!(
            storage
                .suite_dir(Suite::Sql)
                .join("attempts/1/raw/job.json")
                .exists()
        );
        assert_eq!(
            fs::read_to_string(storage.suite_dir(Suite::Sql).join("summary.json")).unwrap(),
            "two"
        );
    }

    #[test]
    fn immutable_evidence_accepts_identical_replay_but_rejects_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run/attempt/raw.json");
        write_string_immutable(&path, "one").unwrap();
        write_string_immutable(&path, "one").unwrap();
        let error = write_string_immutable(&path, "two").unwrap_err();
        assert!(matches!(error, AppError::Integrity(_)));
        assert_eq!(fs::read_to_string(path).unwrap(), "one");
    }
}
