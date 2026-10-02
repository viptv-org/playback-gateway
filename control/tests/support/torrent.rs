use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

// A fixture-only BEP 3 seeder keeps its connection open until the downloader
// requests blocks; a byte-exact handshake supplies deterministic readiness.
pub async fn seed_peer(
    address: std::net::SocketAddr,
    hash: [u8; 20],
    bytes: Vec<u8>,
    block_delay: Duration,
) {
    let mut handshake = Vec::from(&b"\x13BitTorrent protocol"[..]);
    handshake.extend_from_slice(&[0; 8]);
    handshake.extend_from_slice(&hash);
    const PEER_ID: &[u8; 20] = b"fixture-peer-0000001";
    handshake.extend_from_slice(PEER_ID);
    let mut peer = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(mut peer) = TcpStream::connect(address).await {
                let mut reply = [0; 68];
                if peer.write_all(&handshake).await.is_ok()
                    && peer.read_exact(&mut reply).await.is_ok()
                {
                    assert!(reply[28..48] == hash, "fixture handshake must match");
                    break peer;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual service torrent handshake deadline");
    peer.set_nodelay(true).unwrap();
    let pieces = bytes.len().div_ceil(16 * 1024);
    let mut bitfield = vec![0xff; pieces.div_ceil(8)];
    if !pieces.is_multiple_of(8) {
        *bitfield.last_mut().unwrap() <<= 8 - pieces % 8;
    }
    peer.write_u32((bitfield.len() + 1) as u32).await.unwrap();
    peer.write_u8(5).await.unwrap();
    peer.write_all(&bitfield).await.unwrap();
    peer.write_all(&[0, 0, 0, 1, 1]).await.unwrap(); // Unchoke.
    while let Ok(size) = peer.read_u32().await {
        if size == 0 {
            continue;
        }
        assert!(size <= 64 * 1024, "fixture peer message bound");
        let mut message = vec![0; size as usize];
        if peer.read_exact(&mut message).await.is_err() {
            break;
        }
        if message[0] != 6 {
            continue;
        }
        assert_eq!(message.len(), 13, "fixture request shape");
        let index = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
        let offset = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
        assert!(length <= 16 * 1024, "fixture block bound");
        let begin = index * 16 * 1024 + offset;
        let block = bytes
            .get(begin..begin + length)
            .expect("fixture requested payload range");
        if peer.write_u32((length + 9) as u32).await.is_err() {
            break;
        }
        if peer.write_u8(7).await.is_err() {
            break;
        }
        if peer.write_all(&message[1..9]).await.is_err() {
            break;
        }
        if peer.write_all(block).await.is_err() {
            break;
        }
        if !block_delay.is_zero() {
            tokio::time::sleep(block_delay).await;
        }
    }
}
