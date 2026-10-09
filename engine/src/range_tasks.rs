//! Fenced HTTP task ownership using Tokio's tracked-future destruction receipt.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::{
    task::JoinHandle,
    time::{timeout_at, Instant},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Clone)]
pub(crate) struct OwnedTasks(Arc<Owner>);
struct Owner {
    closed: Mutex<bool>,
    failed: AtomicBool,
    token: CancellationToken,
    tasks: TaskTracker,
}
impl OwnedTasks {
    pub fn new(token: CancellationToken) -> Self {
        Self(Arc::new(Owner {
            closed: Mutex::new(false),
            failed: AtomicBool::new(false),
            token,
            tasks: TaskTracker::new(),
        }))
    }
    pub fn spawn(
        &self,
        _span: tracing::Span,
        future: impl std::future::Future<Output = Result<(), std::io::Error>> + Send + 'static,
    ) -> Option<JoinHandle<()>> {
        let closed = self.0.closed.lock().unwrap_or_else(|e| e.into_inner());
        if *closed || self.0.token.is_cancelled() {
            return None;
        }
        let token = self.0.token.clone();
        Some(self.0.tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = token.cancelled() => {},
                _ = future => {},
            }
        }))
    }
    pub fn cancel(&self) {
        let mut closed = self.0.closed.lock().unwrap_or_else(|e| e.into_inner());
        *closed = true;
        self.0.tasks.close();
        self.0.token.cancel();
    }
    pub async fn cancel_and_wait_until(&self, deadline: Instant) -> bool {
        self.cancel();
        let settled = timeout_at(deadline, self.0.tasks.wait()).await.is_ok();
        if !settled {
            self.0.failed.store(true, Ordering::Release);
        }
        settled
    }
    pub fn has_pending_work(&self) -> bool {
        !self.0.tasks.is_empty()
    }
    pub fn has_failed_settlement(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_waits_for_captured_io_destruction_and_fences_new_tasks() {
        struct IoOwner {
            entered: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for IoOwner {
            fn drop(&mut self) {
                tokio::task::block_in_place(|| {
                    let _ = self.entered.take().unwrap().send(());
                    self.release.recv_timeout(Duration::from_secs(3)).unwrap();
                });
            }
        }
        let tasks = OwnedTasks::new(Default::default());
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (release, releasing) = std::sync::mpsc::channel();
        let io = IoOwner {
            entered: Some(entered),
            release: releasing,
        };
        let task = tasks
            .spawn(tracing::Span::none(), async move {
                let _io = io;
                std::future::pending::<()>().await;
                Ok(())
            })
            .unwrap();
        tasks.cancel();
        entering.await.unwrap();
        assert!(tasks
            .spawn(tracing::Span::none(), async { Ok(()) })
            .is_none());
        assert!(
            !tasks
                .cancel_and_wait_until(Instant::now() + Duration::from_millis(10))
                .await
        );
        assert!(tasks.has_pending_work());
        release.send(()).unwrap();
        task.await.unwrap();
        assert!(
            tasks
                .cancel_and_wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
        assert!(!tasks.has_pending_work());
        assert!(
            tasks.has_failed_settlement(),
            "late completion must preserve a failed receipt"
        );
    }
}
