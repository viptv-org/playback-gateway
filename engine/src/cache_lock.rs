//! Non-blocking exclusive ownership of a gateway cache directory.
use std::fs::File;

pub(crate) const CONFLICT: &str = "Torrent cache already in use";
pub(crate) const UNAVAILABLE: &str = "Torrent cache lock unavailable";
const INTERRUPTED_ATTEMPTS: usize = 8;

pub(crate) fn acquire(file: &File) -> Result<(), String> {
    lock_with(file, native_attempt)
}

fn lock_with(file: &File, attempt: impl Fn(&File) -> Attempt) -> Result<(), String> {
    let mut interrupted = 0;
    loop {
        match attempt(file) {
            Attempt::Locked => return Ok(()),
            Attempt::Conflict => return Err(CONFLICT.into()),
            Attempt::Interrupted if interrupted < INTERRUPTED_ATTEMPTS => interrupted += 1,
            Attempt::Interrupted | Attempt::Failed => return Err(UNAVAILABLE.into()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    Locked,
    Conflict,
    Interrupted,
    Failed,
}

impl From<std::io::Error> for Attempt {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::Interrupted {
            Self::Interrupted
        } else {
            Self::Failed
        }
    }
}

#[cfg(target_os = "android")]
fn native_attempt(file: &File) -> Attempt {
    flock(file).map_or(Attempt::Locked, Attempt::from)
}

#[cfg(not(target_os = "android"))]
fn native_attempt(file: &File) -> Attempt {
    match file.try_lock() {
        Ok(()) => Attempt::Locked,
        Err(std::fs::TryLockError::WouldBlock) => Attempt::Conflict,
        Err(std::fs::TryLockError::Error(error)) => Attempt::from(error),
    }
}

#[cfg(any(target_os = "android", all(test, unix)))]
fn flock(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: file owns a valid descriptor for the entire call. flock neither
    // consumes it nor accesses Rust memory; closing its last alias releases it.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs::OpenOptions};

    fn cache_lock(name: &str) -> (tempfile::TempDir, File, File) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let competing = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        (root, owner, competing)
    }

    #[test]
    fn interrupted_lock_is_retried_until_it_is_taken() {
        let (_root, owner, _competing) = cache_lock("cache-lock");
        let attempts = Cell::new(0);
        let result = lock_with(&owner, |_| {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 4 {
                Attempt::Interrupted
            } else {
                Attempt::Locked
            }
        });
        assert_eq!(result, Ok(()));
        assert_eq!(attempts.get(), 4);
    }

    #[test]
    fn only_an_interrupted_lock_is_retried() {
        fn attempt_of(kind: std::io::ErrorKind) -> Attempt {
            Attempt::from(std::io::Error::from(kind))
        }
        assert_eq!(
            attempt_of(std::io::ErrorKind::Interrupted),
            Attempt::Interrupted
        );
        assert_eq!(
            attempt_of(std::io::ErrorKind::WouldBlock),
            Attempt::Failed
        );
        assert_eq!(
            attempt_of(std::io::ErrorKind::PermissionDenied),
            Attempt::Failed
        );
    }

    #[test]
    fn only_a_real_conflict_is_reported_as_in_use() {
        let (_root, owner, _competing) = cache_lock("cache-lock");
        assert_eq!(
            lock_with(&owner, |_| Attempt::Conflict),
            Err(CONFLICT.to_owned())
        );
        assert_eq!(
            lock_with(&owner, |_| Attempt::Failed),
            Err(UNAVAILABLE.to_owned())
        );
    }

    #[test]
    fn persistent_interruption_is_bounded_and_never_claims_a_conflict() {
        let (_root, owner, _competing) = cache_lock("cache-lock");
        let attempts = Cell::new(0);
        let result = lock_with(&owner, |_| {
            attempts.set(attempts.get() + 1);
            Attempt::Interrupted
        });
        assert_eq!(result, Err(UNAVAILABLE.to_owned()));
        assert_eq!(attempts.get(), INTERRUPTED_ATTEMPTS + 1);
    }

    #[test]
    fn exclusive_ownership_is_nonblocking_and_released_with_its_owner() {
        let (_root, owner, competing) = cache_lock("cache-lock");
        acquire(&owner).unwrap();
        assert_eq!(acquire(&competing), Err(CONFLICT.to_owned()));
        let alias = owner.try_clone().unwrap();
        drop(owner);
        assert_eq!(acquire(&competing), Err(CONFLICT.to_owned()));
        drop(alias);
        acquire(&competing).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn android_flock_fallback_distinguishes_a_conflict() {
        let (_root, owner, competing) = cache_lock("cache-lock");
        flock(&owner).unwrap();
        assert_eq!(
            flock(&competing).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}