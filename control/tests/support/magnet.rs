//! Deterministic BEP 5/9/10 fixture peers; never part of the production service.
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
};

#[derive(Clone)]
enum Value {
    Bytes(Vec<u8>),
    Int(u64),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}
impl Value {
    fn bytes(&self) -> &[u8] {
        if let Self::Bytes(v) = self {
            v
        } else {
            panic!("fixture bencode bytes required")
        }
    }
    fn int(&self) -> u64 {
        if let Self::Int(v) = self {
            *v
        } else {
            panic!("fixture bencode integer required")
        }
    }
    fn get(&self, key: &[u8]) -> &Self {
        if let Self::Dict(v) = self {
            v.get(key).expect("fixture bencode field missing")
        } else {
            panic!("fixture bencode dictionary required")
        }
    }
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Bytes(v) => {
                out.extend_from_slice(format!("{}:", v.len()).as_bytes());
                out.extend_from_slice(v);
            }
            Self::Int(v) => out.extend_from_slice(format!("i{v}e").as_bytes()),
            Self::List(v) => {
                out.push(b'l');
                for x in v {
                    x.encode(out)
                }
                out.push(b'e');
            }
            Self::Dict(v) => {
                out.push(b'd');
                for (k, x) in v {
                    Self::Bytes(k.clone()).encode(out);
                    x.encode(out)
                }
                out.push(b'e');
            }
        }
    }
}
fn dict(entries: Vec<(&[u8], Value)>) -> Value {
    Value::Dict(entries.into_iter().map(|(k, v)| (k.to_vec(), v)).collect())
}
fn bytes(v: &[u8]) -> Value {
    Value::Bytes(v.to_vec())
}
fn parse(input: &[u8], at: &mut usize, depth: usize) -> Value {
    assert!(
        depth < 16 && input.len() <= 2 * 1024 * 1024,
        "fixture bencode bound"
    );
    match input.get(*at).copied().expect("fixture bencode truncated") {
        b'i' => {
            *at += 1;
            let start = *at;
            while input[*at] != b'e' {
                *at += 1;
            }
            let n = std::str::from_utf8(&input[start..*at])
                .unwrap()
                .parse()
                .unwrap();
            *at += 1;
            Value::Int(n)
        }
        b'l' => {
            *at += 1;
            let mut v = vec![];
            while input[*at] != b'e' {
                v.push(parse(input, at, depth + 1));
            }
            *at += 1;
            Value::List(v)
        }
        b'd' => {
            *at += 1;
            let mut v = BTreeMap::new();
            while input[*at] != b'e' {
                let k = parse(input, at, depth + 1).bytes().to_vec();
                let x = parse(input, at, depth + 1);
                assert!(v.insert(k, x).is_none());
            }
            *at += 1;
            Value::Dict(v)
        }
        b'0'..=b'9' => {
            let start = *at;
            while input[*at] != b':' {
                *at += 1;
            }
            let n: usize = std::str::from_utf8(&input[start..*at])
                .unwrap()
                .parse()
                .unwrap();
            *at += 1;
            let end = at.checked_add(n).unwrap();
            let v = bytes(input.get(*at..end).expect("fixture bencode byte bound"));
            *at = end;
            v
        }
        _ => panic!("fixture bencode type invalid"),
    }
}
fn decoded(input: &[u8]) -> Value {
    let mut at = 0;
    let v = parse(input, &mut at, 0);
    assert_eq!(at, input.len());
    v
}
fn info_bytes(metainfo: &[u8]) -> Vec<u8> {
    assert_eq!(metainfo[0], b'd');
    let mut at = 1;
    while metainfo[at] != b'e' {
        let k = parse(metainfo, &mut at, 0);
        let start = at;
        parse(metainfo, &mut at, 0);
        if k.bytes() == b"info" {
            return metainfo[start..at].to_vec();
        }
    }
    panic!("fixture metainfo info dictionary missing")
}
#[derive(Default)]
pub struct Stats {
    pub payload: AtomicU64,
    pub metadata: AtomicU64,
    pub connections: AtomicU64,
    pub queries: AtomicU64,
    pub elapsed_millis: AtomicU64,
    pub active: AtomicU64,
}
pub struct Swarm {
    pub stats: Vec<Arc<Stats>>,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Swarm {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn extended(peer: &mut TcpStream, id: u8, payload: &[u8]) -> std::io::Result<()> {
    peer.write_u32((payload.len() + 2) as u32).await?;
    peer.write_all(&[20, id]).await?;
    peer.write_all(payload).await
}
async fn connection(
    mut peer: TcpStream,
    hash: [u8; 20],
    metadata: Arc<Vec<u8>>,
    payload: Arc<Vec<u8>>,
    index: usize,
    delay: Duration,
    stats: Arc<Stats>,
) {
    let mut request = [0; 68];
    if peer.read_exact(&mut request).await.is_err() {
        return;
    }
    assert!(
        request[28..48] == hash,
        "controlled peer handshake must match"
    );
    let mut reply = Vec::from(&b"\x13BitTorrent protocol"[..]);
    let mut reserved = [0; 8];
    reserved[5] = 0x10;
    reply.extend_from_slice(&reserved);
    reply.extend_from_slice(&hash);
    let id = format!("fixture-ext-peer{index:04}");
    assert_eq!(id.len(), 20);
    reply.extend_from_slice(id.as_bytes());
    if peer.write_all(&reply).await.is_err() {
        return;
    }
    stats.connections.fetch_add(1, Ordering::Relaxed);
    stats.active.fetch_add(1, Ordering::Relaxed);
    struct Active(Arc<Stats>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _active = Active(stats.clone());
    let started = std::time::Instant::now();
    let mut header = vec![];
    dict(vec![
        (b"m", dict(vec![(b"ut_metadata", Value::Int(1))])),
        (b"metadata_size", Value::Int(metadata.len() as u64)),
    ])
    .encode(&mut header);
    if extended(&mut peer, 0, &header).await.is_err() {
        return;
    }
    let pieces = payload.len().div_ceil(16 * 1024);
    let mut bitfield = vec![0xff; pieces.div_ceil(8)];
    if !pieces.is_multiple_of(8) {
        *bitfield.last_mut().unwrap() <<= 8 - pieces % 8;
    }
    if peer.write_u32((bitfield.len() + 1) as u32).await.is_err() {
        return;
    }
    if peer.write_u8(5).await.is_err() {
        return;
    }
    if peer.write_all(&bitfield).await.is_err() {
        return;
    }
    if peer.write_all(&[0, 0, 0, 1, 1]).await.is_err() {
        return;
    }
    let mut remote_metadata = None;
    while let Ok(size) = peer.read_u32().await {
        if size == 0 {
            continue;
        }
        assert!(size <= 64 * 1024, "fixture peer message bound");
        let mut message = vec![0; size as usize];
        if peer.read_exact(&mut message).await.is_err() {
            break;
        }
        if message[0] == 20 && message.get(1) == Some(&0) {
            let h = decoded(&message[2..]);
            remote_metadata = Some(h.get(b"m").get(b"ut_metadata").int() as u8);
        } else if message[0] == 20 && message.get(1) == Some(&1) {
            let v = decoded(&message[2..]);
            assert_eq!(v.get(b"msg_type").int(), 0);
            let piece = v.get(b"piece").int() as usize;
            let begin = piece.checked_mul(16 * 1024).unwrap();
            let end = (begin + 16 * 1024).min(metadata.len());
            let block = metadata
                .get(begin..end)
                .expect("fixture metadata piece bound");
            let mut data = vec![];
            dict(vec![
                (b"msg_type", Value::Int(1)),
                (b"piece", Value::Int(piece as u64)),
                (b"total_size", Value::Int(metadata.len() as u64)),
            ])
            .encode(&mut data);
            data.extend_from_slice(block);
            if extended(
                &mut peer,
                remote_metadata.expect("peer extension handshake required"),
                &data,
            )
            .await
            .is_err()
            {
                break;
            }
            stats
                .metadata
                .fetch_add(block.len() as u64, Ordering::Relaxed);
        } else if message[0] == 6 {
            assert_eq!(message.len(), 13);
            let piece = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
            let offset = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
            let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
            assert!(length <= 16 * 1024);
            let begin = piece * 16 * 1024 + offset;
            let block = payload
                .get(begin..begin + length)
                .expect("fixture payload request bound");
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
            stats.payload.fetch_add(length as u64, Ordering::Relaxed);
            stats
                .elapsed_millis
                .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            tokio::time::sleep(delay).await;
        }
    }
}
/// The standard library bootstrap DNS names must map to the guarded alias inside
/// Docker network-none; no host networking or public DHT is used.
pub async fn start(
    hash: [u8; 20],
    metainfo: Vec<u8>,
    payload: Vec<u8>,
    peers: usize,
    delay: Duration,
) -> Swarm {
    assert!((1..=4).contains(&peers));
    let metadata = Arc::new(info_bytes(&metainfo));
    let payload = Arc::new(payload);
    let mut tasks = vec![];
    let mut stats = vec![];
    let mut addresses = vec![];
    for index in 0..peers {
        let listener = TcpListener::bind("11.255.255.1:0").await.unwrap();
        addresses.push(listener.local_addr().unwrap());
        let stat = Arc::new(Stats::default());
        stats.push(stat.clone());
        let m = metadata.clone();
        let p = payload.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let m = m.clone();
                let p = p.clone();
                let s = stat.clone();
                tokio::spawn(connection(stream, hash, m, p, index, delay, s));
            }
        }));
    }
    for port in [6881, 25401] {
        let socket = UdpSocket::bind((std::net::Ipv4Addr::new(11, 255, 255, 1), port))
            .await
            .unwrap();
        let peers = addresses.clone();
        let stat = stats[0].clone();
        tasks.push(tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                let Ok((n, remote)) = socket.recv_from(&mut buffer).await else {
                    break;
                };
                let request = decoded(&buffer[..n]);
                if request.get(b"y").bytes() != b"q" {
                    continue;
                }
                let query = request.get(b"q").bytes();
                let node = [0x51; 20];
                let mut fields = vec![(b"id".as_slice(), bytes(&node))];
                if query == b"find_node" {
                    let mut compact = node.to_vec();
                    compact.extend_from_slice(&[11, 255, 255, 1]);
                    compact.extend_from_slice(&port.to_be_bytes());
                    fields.push((b"nodes", bytes(&compact)));
                } else if query == b"get_peers" {
                    assert!(
                        request.get(b"a").get(b"info_hash").bytes() == hash,
                        "controlled DHT lookup must match"
                    );
                    stat.queries.fetch_add(1, Ordering::Relaxed);
                    fields.push((b"token", bytes(b"fixture")));
                    let values = peers
                        .iter()
                        .map(|a| {
                            let mut c = vec![11, 255, 255, 1];
                            c.extend_from_slice(&a.port().to_be_bytes());
                            Value::Bytes(c)
                        })
                        .collect();
                    fields.push((b"values", Value::List(values)));
                } else if query != b"ping" && query != b"announce_peer" {
                    continue;
                }
                let mut response = vec![];
                dict(vec![
                    (b"r", dict(fields)),
                    (b"t", bytes(request.get(b"t").bytes())),
                    (b"y", bytes(b"r")),
                ])
                .encode(&mut response);
                let _ = socket.send_to(&response, remote).await;
            }
        }));
    }
    Swarm { stats, tasks }
}
