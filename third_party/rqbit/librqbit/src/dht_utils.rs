use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
};

use anyhow::Context;
use buffers::ByteBufOwned;
use futures::{stream::FuturesUnordered, Stream, StreamExt};
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use tracing::{debug, error_span, Instrument};

use crate::{
    peer_connection::PeerConnectionOptions, peer_info_reader, spawn_utils::BlockingSpawner,
    stream_connect::StreamConnector,
};
use librqbit_core::hash_id::Id20;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ReadMetainfoResult<Rx> {
    Found {
        peer: SocketAddr,
        info: TorrentMetaV1Info<ByteBufOwned>,
        info_bytes: ByteBufOwned,
        rx: Rx,
        seen: HashSet<SocketAddr>,
    },
    ChannelClosed {
        #[allow(dead_code)]
        seen: HashSet<SocketAddr>,
    },
}

pub async fn read_metainfo_from_peer_receiver<A: Stream<Item = SocketAddr> + Unpin>(
    peer_id: Id20,
    info_hash: Id20,
    initial_addrs: Vec<SocketAddr>,
    addrs_stream: A,
    peer_connection_options: Option<PeerConnectionOptions>,
    connector: Arc<StreamConnector>,
) -> ReadMetainfoResult<A> {
    let mut seen = HashSet::<SocketAddr>::new();
    let mut attempts = HashMap::<SocketAddr, u8>::new();
    let mut inflight = HashSet::<SocketAddr>::new();
    let max_attempts = if connector.network_policy.is_native() {
        3
    } else {
        1
    };
    let mut addrs = addrs_stream;

    let semaphore = tokio::sync::Semaphore::new(128);

    let read_info_guarded = |addr| {
        let semaphore = &semaphore;
        let connector = connector.clone();
        async move {
            let token = semaphore
                .acquire()
                .await
                .expect("metadata semaphore is not closed");
            let ret = peer_info_reader::read_metainfo_from_peer(
                addr,
                peer_id,
                info_hash,
                peer_connection_options,
                BlockingSpawner::new(true),
                connector,
            )
            .instrument(error_span!("read_metainfo_from_peer", ?addr))
            .await
            .with_context(|| format!("error reading metainfo from {addr}"));
            drop(token);
            (addr, ret)
        }
    };

    let mut unordered = FuturesUnordered::new();

    for a in initial_addrs {
        if seen.insert(a) {
            attempts.insert(a, 1);
            inflight.insert(a);
            unordered.push(read_info_guarded(a));
        }
    }

    let mut addrs_completed = false;

    loop {
        if addrs_completed && unordered.is_empty() {
            return ReadMetainfoResult::ChannelClosed { seen };
        }

        tokio::select! {
            done = unordered.next(), if !unordered.is_empty() => {
                match done {
                    Some((peer, result)) => {
                        inflight.remove(&peer);
                        match result {
                            Ok((info, info_bytes)) => return ReadMetainfoResult::Found { peer, info, info_bytes, seen, rx: addrs },
                            Err(e) => debug!("{:#}", e),
                        }
                    },
                    None => unreachable!()
                }
            }

            next_addr = addrs.next(), if !addrs_completed => {
                match next_addr {
                    Some(addr) => {
                        // Rediscovery can recover transient native connection/handshake
                        // failures. Never duplicate an active attempt or retry forever.
                        let tried = attempts.entry(addr).or_default();
                        if *tried < max_attempts && inflight.insert(addr) {
                            *tried += 1;
                            seen.insert(addr);
                            unordered.push(read_info_guarded(addr));
                        }
                        continue;
                    },
                    None => {
                        addrs_completed = true;
                    },
                }
            }
        };
    }
}

#[cfg(all(test, feature = "test-network-policy"))]
mod metadata_retry_tests;

#[cfg(test)]
mod tests {
    use dht::{DhtBuilder, Id20};
    use librqbit_core::peer_id::generate_peer_id;

    use super::*;
    use std::{
        str::FromStr,
        sync::{Arc, Once},
    };

    static LOG_INIT: Once = Once::new();

    fn init_logging() {
        #[allow(unused_must_use)]
        LOG_INIT.call_once(|| {
            // pretty_env_logger::try_init();
        })
    }

    #[tokio::test]
    #[ignore]
    async fn read_metainfo_from_dht() {
        init_logging();

        let info_hash = Id20::from_str("cab507494d02ebb1178b38f2e9d7be299c86b862").unwrap();
        let dht = DhtBuilder::new().await.unwrap();

        let peer_rx = dht.get_peers(info_hash, None);
        let peer_id = generate_peer_id(b"-xx1234-");
        match read_metainfo_from_peer_receiver(
            peer_id,
            info_hash,
            Vec::new(),
            peer_rx,
            None,
            Arc::new(Default::default()),
        )
        .await
        {
            ReadMetainfoResult::Found { info, .. } => dbg!(info),
            ReadMetainfoResult::ChannelClosed { .. } => todo!("should not have happened"),
        };
    }
}
