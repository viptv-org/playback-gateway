//! Seeder startup must recover when an early listener accepts but never replies.
#[allow(dead_code)]
#[path = "support/torrent.rs"]
mod torrent_fixture;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn seeder_retries_an_accepted_but_unresponsive_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut early, _) = listener.accept().await.unwrap();
        let mut h = [0u8; 68];
        early.read_exact(&mut h).await.unwrap();
        // Keep the first transport open: recovery must bound this attempt.
        let mut ready = loop {
            let (mut candidate, _) = listener.accept().await.unwrap();
            if candidate.read_exact(&mut h).await.is_ok() {
                break candidate;
            }
            // Timed-out attempts may already be queued at accept; retain the
            // ready listener until a live retry actually sends its handshake.
        };
        ready.write_all(&h).await.unwrap();
        let mut input = [0u8; 11];
        ready.read_exact(&mut input).await.unwrap();
        assert_eq!(input[4], 5, "successful retry must send its bitfield");
        drop(early);
    });
    tokio::time::timeout(
        Duration::from_secs(4),
        torrent_fixture::seed_peer(address, [3u8; 20], vec![0u8; 1024], Duration::ZERO),
    )
    .await
    .expect("seeder must retry the early unresponsive listener within the bounded deadline");
    server.await.unwrap();
}
