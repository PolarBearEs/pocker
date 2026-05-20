use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

use crate::error::Result;

pub fn ensure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_directory(parent)?;
    }
    let mut temp = NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_data()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    atomic_write_bytes(path, &bytes)
}

pub fn read_json_if_exists<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map(Some).map_err(Into::into)
}

pub fn reconcile_partial_file(path: &Path, durable_offset: u64) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let file_len = fs::metadata(path)?.len();
    if file_len > durable_offset {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(durable_offset)?;
        return Ok(durable_offset);
    }
    Ok(file_len)
}
