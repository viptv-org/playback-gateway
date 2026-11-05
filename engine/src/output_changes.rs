//! Wake startup checks when FFmpeg publishes output, without trusting the event
//! as proof of playable media. A finite poll also covers child exits, exhausted
//! watch limits, lost events and platforms without inotify.
use std::{path::Path, time::Duration};

const FALLBACK_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(target_os = "linux")]
enum Publication {
    Changed,
    WatchBroken,
    NoWatch,
}

pub(super) struct OutputChanges {
    #[cfg(target_os = "linux")]
    fd: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
}

impl OutputChanges {
    /// Register before spawning the producer so a fast first segment is not
    /// lost between checking the filesystem and installing the watch.
    pub(super) fn new(path: &Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            use std::{ffi::CString, os::fd::FromRawFd, os::unix::ffi::OsStrExt};
            let fd = (|| {
                let path = CString::new(path.as_os_str().as_bytes()).ok()?;
                // SAFETY: no borrowed memory is retained; OwnedFd closes every
                // failure path, including registration with the async reactor.
                let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
                if raw < 0 {
                    return None;
                }
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
                let mask = libc::IN_CLOSE_WRITE
                    | libc::IN_MOVED_TO
                    | libc::IN_CREATE
                    | libc::IN_DELETE_SELF;
                if unsafe { libc::inotify_add_watch(raw, path.as_ptr(), mask) } < 0 {
                    return None;
                }
                tokio::io::unix::AsyncFd::new(fd).ok()
            })();
            Self { fd }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Self {}
        }
    }

    #[cfg(target_os = "linux")]
    async fn publication(&self) -> Publication {
        use std::os::fd::AsRawFd;
        let Some(fd) = &self.fd else {
            return Publication::NoWatch;
        };
        loop {
            let mut ready = match fd.readable().await {
                Ok(ready) => ready,
                Err(_) => return Publication::WatchBroken,
            };
            let result = ready.try_io(|fd| {
                // Event contents are deliberately ignored. The caller
                // rechecks its existing cache and playback predicates.
                let mut buffer = [0u8; 4096];
                let count = unsafe {
                    libc::read(
                        fd.get_ref().as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if count < 0 {
                    Err(std::io::Error::last_os_error())
                } else if count == 0 {
                    Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
                } else {
                    Ok(())
                }
            });
            match result {
                Ok(Ok(())) => return Publication::Changed,
                Ok(Err(_)) => return Publication::WatchBroken,
                Err(_) => continue,
            }
        }
    }

    pub(super) async fn wait(&mut self) {
        #[cfg(target_os = "linux")]
        if self.fd.is_some() {
            let publication = tokio::time::timeout(FALLBACK_INTERVAL, self.publication()).await;
            if matches!(publication, Ok(Publication::WatchBroken)) {
                self.fd = None;
                // A broken descriptor must not become a busy retry loop.
                tokio::time::sleep(FALLBACK_INTERVAL).await;
            }
            return;
        }
        tokio::time::sleep(FALLBACK_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn publication_before_wait_is_not_lost_and_readiness_is_consumed() {
        let root = tempfile::tempdir().unwrap();
        let mut changes = OutputChanges::new(root.path());
        if changes.fd.is_none() {
            eprintln!("inotify watch unavailable; the publication path is not exercised");
            return;
        }
        std::fs::write(root.path().join("index.m3u8.tmp"), "#EXTM3U\n").unwrap();
        std::fs::rename(
            root.path().join("index.m3u8.tmp"),
            root.path().join("index.m3u8"),
        )
        .unwrap();
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(10), changes.wait())
            .await
            .expect("a published playlist must wake the wait");
        tokio::time::pause();
        // Drain any coalesced kernel events before testing the idle path.
        let mut buffer = [0u8; 4096];
        use std::os::fd::AsRawFd;
        while unsafe {
            libc::read(
                changes.fd.as_ref().unwrap().get_ref().as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        } > 0
        {}
        let started = tokio::time::Instant::now();
        changes.wait().await;
        assert!(started.elapsed() >= FALLBACK_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_watch_uses_finite_polling() {
        let root = tempfile::tempdir().unwrap();
        let mut changes = OutputChanges::new(&root.path().join("missing"));
        let started = tokio::time::Instant::now();
        changes.wait().await;
        assert!(started.elapsed() >= FALLBACK_INTERVAL);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn cancellation_leaves_watcher_usable() {
        let root = tempfile::tempdir().unwrap();
        let mut changes = OutputChanges::new(root.path());
        if changes.fd.is_none() {
            eprintln!("inotify watch unavailable; the publication path is not exercised");
            return;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(1), changes.wait())
                .await
                .is_err(),
            "an idle watch must not report a publication"
        );
        tokio::time::resume();
        std::fs::write(root.path().join("segment-000.ts"), [0x47; 188]).unwrap();
        tokio::time::timeout(Duration::from_secs(10), changes.wait())
            .await
            .expect("a cancelled wait must leave the watch usable");
    }
}
