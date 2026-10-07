#![cfg(feature = "torrent")]

use librqbit::{OwnedTasks, Settlement};
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test]
async fn closed_parent_fences_child_and_queued_work_retains_both_receipts() {
    let root = OwnedTasks::new(Default::default());
    let child = root.child();
    let queued = child
        .wrap_blocking(|| panic!("retired queue must not execute"))
        .unwrap();
    assert!(root.has_pending_work());
    assert_eq!(
        child
            .cancel_and_settle_until(Instant::now() + Duration::from_millis(10))
            .await,
        Settlement::SettlementFailed
    );
    assert!(child.has_failed_settlement());
    assert!(root.has_failed_settlement());
    assert!(child.register().is_none());
    assert!(child.wrap_blocking(|| {}).is_none());
    assert!(root.has_pending_work());
    drop(queued);
    assert_eq!(
        child
            .cancel_and_settle_until(Instant::now() + Duration::from_secs(2))
            .await,
        Settlement::Settled
    );
    assert!(
        child.has_failed_settlement(),
        "delayed completion must not clear poison"
    );
    root.cancel();
    assert!(root.register().is_none());
    assert!(root.child().register().is_none());
    assert!(!root.has_pending_work());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_awaiter_keeps_spawn_blocking_closure_owned_until_actual_exit() {
    let root = OwnedTasks::new(Default::default());
    let child = root.child();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let work = child
        .wrap_blocking(move || {
            let _ = entered_tx.send(());
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        })
        .unwrap();
    let awaiter = child
        .spawn(tracing::Span::none(), async move {
            tokio::task::spawn_blocking(work).await?;
            Ok(())
        })
        .unwrap();
    entered_rx.await.unwrap();
    awaiter.abort();
    let _ = awaiter.await;
    assert_eq!(
        child
            .cancel_and_settle_until(Instant::now() + Duration::from_millis(10))
            .await,
        Settlement::SettlementFailed
    );
    assert!(child.has_pending_work());
    assert!(root.has_pending_work());
    release_tx.send(()).unwrap();
    assert_eq!(
        child
            .cancel_and_settle_until(Instant::now() + Duration::from_secs(2))
            .await,
        Settlement::Settled
    );
    assert!(!root.has_pending_work());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_receipt_waits_for_worker_resource_destructors() {
    struct Destructor {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for Destructor {
        fn drop(&mut self) {
            tokio::task::block_in_place(|| {
                let _ = self.entered.take().unwrap().send(());
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            });
        }
    }
    let root = OwnedTasks::new(Default::default());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let destructor = Destructor {
        entered: Some(entered_tx),
        release: release_rx,
    };
    root.spawn(tracing::Span::none(), async move {
        let _resource = destructor;
        std::future::pending::<()>().await;
        Ok(())
    })
    .unwrap();
    tokio::task::yield_now().await;
    root.cancel();
    entered_rx.await.unwrap();
    assert!(root.has_pending_work());
    assert_eq!(
        root.cancel_and_settle_until(Instant::now() + Duration::from_millis(10))
            .await,
        Settlement::SettlementFailed
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        root.cancel_and_settle_until(Instant::now() + Duration::from_secs(2))
            .await,
        Settlement::Settled
    );
}
