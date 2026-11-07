//! Port of `sidepulse/log_rotation.py`: locked, size-rotated JSONL appends.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::pyjson::compact;

pub const HOOK_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const HOOK_LOG_BACKUP_COUNT: u32 = 3;
pub const AUDIT_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const AUDIT_LOG_BACKUP_COUNT: u32 = 3;

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

struct FileLock(File);

impl FileLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        // SAFETY: valid open descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: valid open descriptor; closing would also release the lock.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn append_json_record(
    path: &Path,
    record: &Value,
    max_bytes: u64,
    backup_count: u32,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut payload = compact(record);
    payload.push('\n');
    let _lock = FileLock::acquire(&sibling(path, ".lock"))?;
    rotate_before_append(path, payload.len() as u64, max_bytes, backup_count)?;
    let mut handle = OpenOptions::new().append(true).create(true).open(path)?;
    handle.write_all(payload.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn rotate_before_append(
    path: &Path,
    incoming: u64,
    max_bytes: u64,
    backup_count: u32,
) -> io::Result<bool> {
    if max_bytes == 0 || backup_count == 0 {
        return Ok(false);
    }
    let current = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if current + incoming <= max_bytes {
        return Ok(false);
    }
    let _ = fs::remove_file(sibling(path, &format!(".{backup_count}")));
    for index in (1..backup_count).rev() {
        let source = sibling(path, &format!(".{index}"));
        if source.exists() {
            fs::rename(&source, sibling(path, &format!(".{}", index + 1)))?;
        }
    }
    fs::rename(path, sibling(path, ".1"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sub/x.jsonl");
        for i in 0..5 {
            append_json_record(&log, &json!({"i": i, "pad": "x".repeat(20)}), 64, 2).unwrap();
        }
        assert!(log.exists());
        assert!(sibling(&log, ".1").exists());
        assert!(sibling(&log, ".2").exists());
        assert!(!sibling(&log, ".3").exists());
        let mode = fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    }
}
