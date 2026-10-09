//! Bounded archive spool ownership, independent of torrent acquisition.
use crate::input::MIN_CACHE_BYTES;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct ArchiveCache(Arc<Cache>);
struct Cache {
    path: PathBuf,
    enabled: bool,
    max_bytes: u64,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    lock: Option<File>,
    reserved: u64,
    retired: Vec<(PathBuf, u64)>,
}
pub struct ArchiveCacheReservation {
    cache: Arc<Cache>,
    folder: PathBuf,
    bytes: u64,
}
impl ArchiveCache {
    pub fn new(path: PathBuf, enabled: bool, max_bytes: u64) -> Result<Self, String> {
        if max_bytes < MIN_CACHE_BYTES {
            return Err("Invalid torrent configuration".into());
        }
        Ok(Self(Arc::new(Cache {
            path,
            enabled,
            max_bytes,
            state: Mutex::new(State::default()),
        })))
    }
    pub async fn reserve_cache(&self, bytes: u64) -> Result<ArchiveCacheReservation, String> {
        let cache = self.0.clone();
        tokio::task::spawn_blocking(move || {
            if !cache.enabled {
                return Err("Torrent transport disabled".into());
            }
            let mut state = cache.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.lock.is_none() {
                state.lock = Some(prepare(&cache.path)?);
            }
            cleanup(&mut state)?;
            let reserved = state
                .reserved
                .checked_add(bytes)
                .filter(|bytes| *bytes <= cache.max_bytes)
                .ok_or("Torrent cache capacity reached")?;
            let folder = cache
                .path
                .join(format!("archive-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir(&folder).map_err(|_| "Torrent cache unavailable")?;
            if let Err(error) = private_directory(&folder) {
                let _ = std::fs::remove_dir(&folder);
                return Err(error);
            }
            state.reserved = reserved;
            drop(state);
            Ok(ArchiveCacheReservation {
                cache,
                folder,
                bytes,
            })
        })
        .await
        .map_err(|_| "Torrent cache unavailable")?
    }
    pub async fn reap(&self) -> Result<(), String> {
        let cache = self.0.clone();
        tokio::task::spawn_blocking(move || {
            cleanup(&mut cache.state.lock().unwrap_or_else(|e| e.into_inner()))
        })
        .await
        .map_err(|_| "Torrent cache unavailable")?
    }
}
impl ArchiveCacheReservation {
    pub fn path(&self) -> &Path {
        &self.folder
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
impl Drop for ArchiveCacheReservation {
    fn drop(&mut self) {
        // Blocking readers retain this owner. Dropping an async waiter cannot
        // queue deletion or refund their bytes before those readers exit.
        self.cache
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retired
            .push((self.folder.clone(), self.bytes));
    }
}
fn cleanup(state: &mut State) -> Result<(), String> {
    let mut failed = false;
    state.retired.retain(|(folder, bytes)| {
        if std::fs::remove_dir_all(folder).is_ok() {
            state.reserved -= bytes;
            false
        } else {
            failed = true;
            true
        }
    });
    if failed {
        Err("Torrent cache cleanup failed".into())
    } else {
        Ok(())
    }
}
fn private_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Torrent cache permissions unavailable")?;
    }
    Ok(())
}
fn prepare(path: &Path) -> Result<File, String> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| "Torrent cache unavailable")?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Unsafe torrent cache directory".into());
        }
    }
    std::fs::create_dir_all(path).map_err(|_| "Torrent cache unavailable")?;
    private_directory(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options
        .open(path.join(".archive-lock"))
        .map_err(|_| "Torrent cache lock unavailable")?;
    lock.try_lock()
        .map_err(|_| "Torrent cache already in use")?;
    for entry in std::fs::read_dir(path).map_err(|_| "Torrent cache unavailable")? {
        let entry = entry.map_err(|_| "Torrent cache unavailable")?;
        if entry.file_name().to_string_lossy().starts_with("archive-") {
            let kind = entry.file_type().map_err(|_| "Torrent cache unavailable")?;
            if !kind.is_dir() || kind.is_symlink() {
                return Err("Unsafe torrent cache entry".into());
            }
            std::fs::remove_dir_all(entry.path()).map_err(|_| "Torrent cache cleanup failed")?;
        }
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_spool_is_deleted_before_capacity_is_reused() {
        let root = tempfile::tempdir().unwrap();
        let cache = ArchiveCache::new(root.path().join("archives"), true, MIN_CACHE_BYTES).unwrap();
        let reservation = cache.reserve_cache(MIN_CACHE_BYTES).await.unwrap();
        let folder = reservation.path().to_owned();
        std::fs::write(folder.join("partial.media"), b"unfinished extraction").unwrap();
        assert!(cache.reserve_cache(1).await.is_err());
        drop(reservation);
        let replacement = cache.reserve_cache(MIN_CACHE_BYTES).await.unwrap();
        assert!(!folder.exists());
        drop(replacement);
        cache.reap().await.unwrap();
    }

    #[tokio::test]
    async fn reservation_retains_exclusive_ownership_until_reader_release() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("archives");
        let first = ArchiveCache::new(path.clone(), true, MIN_CACHE_BYTES).unwrap();
        let reservation = first.reserve_cache(100).await.unwrap();
        let folder = reservation.path().to_owned();
        let second = ArchiveCache::new(path, true, MIN_CACHE_BYTES).unwrap();
        drop(first);
        assert!(second.reserve_cache(1).await.is_err());
        drop(reservation);
        let reopened = second.reserve_cache(1).await.unwrap();
        assert!(
            !folder.exists(),
            "restart must reclaim only its abandoned spool"
        );
        drop(reopened);
        second.reap().await.unwrap();
    }
}
