use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Mutex,
};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    day: u64,
    bytes: u64,
}

pub(crate) struct Budget {
    file: Mutex<File>,
    limit: u64,
}

impl Budget {
    pub fn open(path: &Path, limit: u64) -> std::io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() || file.metadata()?.len() > 512 {
            return Err(std::io::Error::other("invalid telemetry ledger"));
        }
        Ok(Self {
            file: Mutex::new(file),
            limit,
        })
    }
    pub fn reserve(&self, day: u64, amount: u64) -> bool {
        let Ok(mut file) = self.file.lock() else {
            return false;
        };
        if file.try_lock_exclusive().is_err() {
            return false;
        }
        let reserved = (|| -> std::io::Result<bool> {
            if file.metadata()?.len() > 512 {
                return Ok(false);
            }
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            (&mut *file).take(513).read_to_end(&mut bytes)?;
            let mut ledger = if bytes.is_empty() {
                Ledger::default()
            } else {
                match serde_json::from_slice::<Ledger>(&bytes) {
                    Ok(ledger) => ledger,
                    Err(_) => return Ok(false),
                }
            };
            if day < ledger.day {
                return Ok(false);
            }
            if day > ledger.day {
                ledger = Ledger { day, bytes: 0 };
            }
            let Some(next) = ledger
                .bytes
                .checked_add(amount)
                .filter(|bytes| *bytes <= self.limit)
            else {
                return Ok(false);
            };
            ledger.bytes = next;
            let bytes = serde_json::to_vec(&ledger)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&bytes)?;
            file.set_len(bytes.len() as u64)?;
            file.sync_data()?;
            Ok(true)
        })()
        .unwrap_or(false);
        let _ = FileExt::unlock(&*file);
        reserved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_process_handles_cannot_bypass_quota_or_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota");
        let first = Budget::open(&path, 100).unwrap();
        let second = Budget::open(&path, 100).unwrap();
        assert!(first.reserve(7, 70));
        assert!(!second.reserve(7, 31));
        let file = first.file.lock().unwrap();
        file.lock_exclusive().unwrap();
        assert!(!second.reserve(7, 1));
        FileExt::unlock(&*file).unwrap();
        drop(file);
        assert!(second.reserve(7, 30));
        assert!(!first.reserve(7, 1));
    }
    #[cfg(unix)]
    #[test]
    fn ledger_rejects_symlinks_and_creates_private_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "unrelated").unwrap();
        let path = dir.path().join("quota");
        symlink(&target, &path).unwrap();
        assert!(Budget::open(&path, 100).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unrelated");
        let private = dir.path().join("private");
        Budget::open(&private, 100).unwrap();
        assert_eq!(
            std::fs::metadata(private).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn persistent_quota_counts_attempts_and_denies_clock_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota");
        let first = Budget::open(&path, 100).unwrap();
        assert!(first.reserve(7, 60));
        assert!(!first.reserve(7, 50));
        drop(first);
        let second = Budget::open(&path, 100).unwrap();
        assert!(!second.reserve(7, 50));
        assert!(!second.reserve(6, 1));
        assert!(second.reserve(8, 100));
        assert!(!second.reserve(8, 1));
    }
    #[test]
    fn malformed_state_never_resets_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota");
        std::fs::write(&path, "malformed").unwrap();
        assert!(!Budget::open(&path, 100).unwrap().reserve(9, 1));
    }
}
