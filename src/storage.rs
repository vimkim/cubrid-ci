use std::fs;
use std::io::Write;
use std::path::Path;

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::error::AppError;

pub fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &bytes)
}

pub fn write_bytes_atomic(path: &Path, value: &[u8]) -> Result<(), AppError> {
    let parent = parent(path)?;
    create_dir_all(parent)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| AppError::storage(parent, source))?;
    write_and_sync(&mut temporary, value)?;
    temporary
        .persist(path)
        .map_err(|error| AppError::storage(path, error.error))?;
    sync_parent(path)
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

fn write_bytes_immutable(path: &Path, value: &[u8]) -> Result<(), AppError> {
    match fs::read(path) {
        Ok(existing) if existing == value => return Ok(()),
        Ok(_) => return Err(immutable_conflict(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(AppError::storage(path, error)),
    }
    let parent = parent(path)?;
    create_dir_all(parent)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| AppError::storage(parent, source))?;
    write_and_sync(&mut temporary, value)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => sync_parent(path),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(path).map_err(|source| AppError::storage(path, source))?;
            if existing == value {
                Ok(())
            } else {
                Err(immutable_conflict(path))
            }
        }
        Err(error) => Err(AppError::storage(path, error.error)),
    }
}

fn write_bytes_if_absent(path: &Path, value: &[u8]) -> Result<(), AppError> {
    if path.exists() {
        return Ok(());
    }
    let parent = parent(path)?;
    create_dir_all(parent)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| AppError::storage(parent, source))?;
    write_and_sync(&mut temporary, value)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => sync_parent(path),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(AppError::storage(path, error.error)),
    }
}

fn write_and_sync(temporary: &mut NamedTempFile, value: &[u8]) -> Result<(), AppError> {
    temporary
        .write_all(value)
        .map_err(|source| AppError::storage(temporary.path(), source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| AppError::storage(temporary.path(), source))
}

fn immutable_conflict(path: &Path) -> AppError {
    AppError::Integrity(format!(
        "immutable evidence already exists with different content: {}",
        path.display()
    ))
}

fn parent(path: &Path) -> Result<&Path, AppError> {
    path.parent()
        .ok_or_else(|| AppError::Input(format!("output path has no parent: {}", path.display())))
}

fn create_dir_all(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(|source| AppError::storage(path, source))
}

fn sync_parent(path: &Path) -> Result<(), AppError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| AppError::storage(parent, source))
}

#[cfg(test)]
mod tests {
    use super::*;

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
