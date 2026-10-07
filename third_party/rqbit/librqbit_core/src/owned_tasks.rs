//! Fenced task ownership. Cancellation is a request; only an empty closed
//! group proves completion. Blocking work remains owned until its poll exits.
use std::sync::{Arc, Mutex};
use tokio::{
    sync::Notify,
    task::JoinHandle,
    time::{timeout_at, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::Span;

#[derive(Clone)]
pub struct OwnedTasks(Arc<Owner>);
struct Owner {
    parent: Option<OwnedTasks>,
    token: CancellationToken,
    state: Mutex<State>,
    changed: Notify,
}
#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
    settlement_failed: bool,
}
/// A deadline is a failed settlement, never a successful cancellation receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    Settled,
    SettlementFailed,
}
/// An operation receipt: dropping an awaiter does not end work that still
/// retains this guard. Register before IO/enqueue, not when awaiting a result.
pub struct OwnedWork {
    _guards: Vec<Guard>,
}
struct Guard(Arc<Owner>);
struct OwnedCall<F> {
    operation: F,
    _work: OwnedWork,
}
impl<F: FnOnce()> OwnedCall<F> {
    fn run(self) {
        let Self { operation, _work } = self;
        operation();
        drop(_work);
    }
}

// Fields drop in declaration order: the entire worker future (and its IO/
// queued resources) is destroyed before its completion receipt is released.
struct OwnedFuture<F> {
    future: std::pin::Pin<Box<F>>,
    _work: OwnedWork,
}
impl<F: std::future::Future> std::future::Future for OwnedFuture<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        if state.active == 0 {
            self.0.changed.notify_waiters();
        }
    }
}
impl OwnedTasks {
    pub fn new(token: CancellationToken) -> Self {
        Self(Arc::new(Owner {
            parent: None,
            token,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }))
    }
    pub fn child(&self) -> Self {
        Self(Arc::new(Owner {
            parent: Some(self.clone()),
            token: self.0.token.child_token(),
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }))
    }
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.0.token
    }
    pub fn register(&self) -> Option<OwnedWork> {
        let mut guards = Vec::new();
        let mut current = Some(self.clone());
        while let Some(group) = current {
            {
                let mut state = group.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.closed || group.0.token.is_cancelled() {
                    return None;
                }
                state.active += 1;
            }
            guards.push(Guard(group.0.clone()));
            current = group.0.parent.clone();
        }
        Some(OwnedWork { _guards: guards })
    }
    pub fn spawn(
        &self,
        span: Span,
        fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> Option<JoinHandle<()>> {
        let work = self.register()?;
        // The future receipt covers synchronous polling/destruction, while a
        // second receipt lasts until Tokio finishes the complete task. The
        // runtime can finish dropping task captures after the inner future's
        // receipt has gone; that must not acknowledge sockets as settled.
        let joined = match self.register() {
            Some(joined) => joined,
            None => {
                drop(fut);
                drop(work);
                return None;
            }
        };
        let token = self.0.token.clone();
        let handle = crate::spawn_utils::spawn(
            span,
            OwnedFuture {
                future: Box::pin(async move {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => Ok(()),
                        result = fut => result,
                    }
                }),
                _work: work,
            },
        );
        let completion = handle.abort_handle();
        // Cancellation deliberately does not cancel this completion waiter.
        // Blocking polls still retain `work`, even during runtime shutdown.
        let _ = crate::spawn_utils::spawn(Span::none(), async move {
            let _joined = joined;
            while !completion.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Ok(())
        });
        Some(handle)
    }
    /// Register before enqueue. Both execution and dropping a queued call
    /// dispose its captured resources before releasing the ownership receipt.
    pub fn wrap_blocking(
        &self,
        operation: impl FnOnce() + Send + Sync + 'static,
    ) -> Option<Box<dyn FnOnce() + Send + Sync>> {
        let call = OwnedCall {
            operation,
            _work: self.register()?,
        };
        Some(Box::new(move || call.run()))
    }
    /// Attach a second owner to the complete future lifetime, including Drop.
    pub fn track_future<F: std::future::Future>(
        &self,
        future: F,
    ) -> Option<impl std::future::Future<Output = F::Output>> {
        Some(OwnedFuture {
            future: Box::pin(future),
            _work: self.register()?,
        })
    }
    /// An idle observation, not a spawn fence. Callers must independently
    /// prevent new consumers; use cancel_and_wait_until for a closed receipt.
    pub async fn wait_idle_until(&self, deadline: Instant) -> bool {
        let settled = timeout_at(deadline, async {
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self.has_pending_work() {
                    return;
                }
                changed.await;
            }
        })
        .await
        .is_ok();
        if !settled {
            self.mark_settlement_failed();
        }
        settled
    }
    pub fn cancel(&self) {
        // Fence before signalling. Every ancestor is checked during admission.
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
        self.0.token.cancel();
        self.0.changed.notify_waiters();
    }
    pub fn has_pending_work(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            != 0
    }
    /// Sticky failure retained by every owner, even after delayed IO exits.
    /// Embedders must fence new native admission until that owner is retired.
    pub fn has_failed_settlement(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settlement_failed
    }
    /// Retain a static failure when an untracked external driver cannot prove
    /// completion. This does not erase or release any active operation.
    pub fn mark_settlement_failed(&self) {
        let mut current = Some(self.clone());
        while let Some(group) = current {
            group
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .settlement_failed = true;
            current = group.0.parent.clone();
        }
    }
    pub async fn cancel_and_settle_until(&self, deadline: Instant) -> Settlement {
        if self.cancel_and_wait_until(deadline).await {
            Settlement::Settled
        } else {
            Settlement::SettlementFailed
        }
    }
    pub async fn cancel_and_wait_until(&self, deadline: Instant) -> bool {
        self.cancel();
        self.wait_until(deadline).await
    }
    pub async fn wait_until(&self, deadline: Instant) -> bool {
        let settled = timeout_at(deadline, async {
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.closed && state.active == 0 {
                        return;
                    }
                }
                changed.await;
            }
        })
        .await
        .is_ok();
        if !settled {
            self.mark_settlement_failed();
        }
        settled
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn owned_tasks_fences_parent_and_child_admission() {
        let root = OwnedTasks::new(CancellationToken::new());
        let child = root.child();
        let work = child.register().unwrap();
        assert!(root.has_pending_work());
        root.cancel();
        assert!(root.register().is_none());
        assert!(child.register().is_none());
        assert!(
            !root
                .wait_until(Instant::now() + Duration::from_millis(5))
                .await
        );
        assert!(root.has_pending_work());
        drop(work);
        assert!(
            root.wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
    }
    #[tokio::test]
    async fn owned_tasks_queued_operation_stays_charged_until_disposed() {
        let root = OwnedTasks::new(CancellationToken::new());
        let child = root.child();
        let queued = child
            .wrap_blocking(|| panic!("cancelled queue must not execute"))
            .unwrap();
        assert!(
            !child
                .cancel_and_wait_until(Instant::now() + Duration::from_millis(5))
                .await
        );
        assert!(root.has_pending_work());
        assert!(child.wrap_blocking(|| {}).is_none());
        drop(queued);
        assert!(
            child
                .wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
        assert!(
            root.cancel_and_wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_tasks_joins_worker_destructors_before_acknowledging_completion() {
        struct BlockOnDrop {
            entered: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for BlockOnDrop {
            fn drop(&mut self) {
                tokio::task::block_in_place(|| {
                    let _ = self.entered.take().unwrap().send(());
                    self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                });
            }
        }
        let root = OwnedTasks::new(CancellationToken::new());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let captured = BlockOnDrop {
            entered: Some(entered_tx),
            release: release_rx,
        };
        let handle = root
            .spawn(Span::none(), async move {
                let _captured = captured;
                std::future::pending::<()>().await;
                Ok(())
            })
            .unwrap();
        tokio::task::yield_now().await;
        root.cancel();
        entered_rx.await.unwrap();
        assert!(
            !root
                .wait_until(Instant::now() + Duration::from_millis(5))
                .await
        );
        release_tx.send(()).unwrap();
        handle.await.unwrap();
        assert!(
            root.wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
    }
    #[tokio::test]
    async fn owned_tasks_child_cancellation_preserves_a_sibling() {
        let root = OwnedTasks::new(CancellationToken::new());
        let a = root.child();
        let b = root.child();
        let work = b.register().unwrap();
        assert!(
            a.cancel_and_wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
        assert!(!b.cancellation_token().is_cancelled());
        assert!(root.has_pending_work());
        drop(work);
        assert!(
            root.cancel_and_wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_tasks_abort_is_not_blocking_work_completion() {
        let root = OwnedTasks::new(CancellationToken::new());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = root
            .spawn(Span::none(), async move {
                tokio::task::block_in_place(|| {
                    let _ = entered_tx.send(());
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                Ok(())
            })
            .unwrap();
        entered_rx.await.unwrap();
        handle.abort();
        assert!(
            !root
                .cancel_and_wait_until(Instant::now() + Duration::from_millis(10))
                .await
        );
        assert!(root.has_pending_work());
        release_tx.send(()).unwrap();
        let _ = handle.await;
        assert!(
            root.wait_until(Instant::now() + Duration::from_secs(1))
                .await
        );
    }
}
