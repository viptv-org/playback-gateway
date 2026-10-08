// Maintained playback-gateway changes: owned cancellation and joined settlement.
use std::{
    cmp::Reverse,
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc,
    },
    task::Poll,
    time::{Duration, Instant},
};

use crate::{
    bprotocol::{
        self, AnnouncePeer, CompactNodeInfo, ErrorDescription, FindNodeRequest, GetPeersRequest,
        Message, MessageKind, Node, PingRequest, Response,
    },
    peer_store::PeerStore,
    routing_table::{InsertResult, NodeStatus, RoutingTable},
    INACTIVITY_TIMEOUT, REQUERY_INTERVAL, RESPONSE_TIMEOUT,
};
use anyhow::{bail, Context};
use backoff::{backoff::Backoff, ExponentialBackoffBuilder};
use bencode::ByteBufOwned;
use dashmap::DashMap;
use futures::{
    future::BoxFuture, stream::FuturesUnordered, FutureExt, Stream, StreamExt, TryFutureExt,
};
use librqbit_core::network_policy::{NetworkPolicy, PUBLIC_BOOTSTRAP};

use leaky_bucket::RateLimiter;
use librqbit_core::{
    crate_version,
    hash_id::Id20,
    owned_tasks::{OwnedTasks, OwnedWork},
    peer_id::generate_azereus_style,
};
use parking_lot::RwLock;

use serde::Serialize;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{channel, unbounded_channel, Sender, UnboundedReceiver, UnboundedSender},
};

use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, error, error_span, info, trace, warn, Instrument};

#[derive(Debug, Serialize)]
pub struct DhtStats {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    pub id: Id20,
    pub outstanding_requests: usize,
    pub routing_table_size: usize,
}

struct RequestSlot<'a> {
    state: &'a DhtState,
    key: (u16, SocketAddr),
}
impl Drop for RequestSlot<'_> {
    fn drop(&mut self) {
        self.state.inflight_by_transaction_id.remove(&self.key);
    }
}
struct OutstandingRequest {
    done: tokio::sync::oneshot::Sender<anyhow::Result<ResponseOrError>>,
}

pub struct WorkerSendRequest {
    // If this is set, we are tracking the response in inflight_by_transaction_id
    our_tid: Option<u16>,
    message: Message<ByteBufOwned>,
    addr: SocketAddr,
    ownership: Option<(OwnedWork, Option<OwnedWork>, CancellationToken)>,
}

#[derive(Debug)]
struct MaybeUsefulNode {
    id: Id20,
    addr: SocketAddr,
    last_request: Instant,
    last_response: Option<Instant>,
    errors_in_a_row: usize,
    returned_peers: bool,
}

fn make_rate_limiter() -> RateLimiter {
    // TODO: move to configuration, i'm lazy.
    let dht_queries_per_second = std::env::var("DHT_QUERIES_PER_SECOND")
        .map(|v| v.parse().expect("couldn't parse DHT_QUERIES_PER_SECOND"))
        .unwrap_or(250usize);

    let per_100_ms = dht_queries_per_second / 10;

    RateLimiter::builder()
        .initial(per_100_ms)
        .max(dht_queries_per_second)
        .interval(Duration::from_millis(100))
        .fair(false)
        .refill(per_100_ms)
        .build()
}

trait RecursiveRequestCallbacks: Sized + Send + Sync + 'static {
    fn on_request_start(&self, req: &RecursiveRequest<Self>, target_node: Id20, addr: SocketAddr);
    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        addr: SocketAddr,
        resp: &anyhow::Result<ResponseOrError>,
    );
}

struct RecursiveRequestCallbacksGetPeers {
    // Id20::from_str("00000fffffffffffffffffffffffffffffffffff").unwrap()
    min_distance_to_announce: Id20,
    announce_port: Option<u16>,
}

impl RecursiveRequestCallbacks for RecursiveRequestCallbacksGetPeers {
    fn on_request_start(&self, _: &RecursiveRequest<Self>, _: Id20, _: SocketAddr) {}

    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        addr: SocketAddr,
        resp: &anyhow::Result<ResponseOrError>,
    ) {
        let announce_port = match self.announce_port {
            Some(a) => a,
            None => return,
        };
        let resp = match resp {
            Ok(ResponseOrError::Response(resp)) => resp,
            _ => return,
        };
        let token = match &resp.token {
            Some(token) => token,
            None => return,
        };
        if req.info_hash.distance(&target_node) > self.min_distance_to_announce {
            trace!(
                "not announcing, {:?} is too far from {:?}",
                target_node,
                req.info_hash
            );
            return;
        }
        let (tid, message) = req.dht.create_request(Request::Announce {
            info_hash: req.info_hash,
            token: token.clone(),
            port: announce_port,
        });

        let Some(work) = req.tasks.register() else {
            return;
        };
        let parent_work = match req.parent_owner.as_ref() {
            Some(parent) => {
                let Some(work) = parent.register() else {
                    return;
                };
                Some(work)
            }
            None => None,
        };
        let _ = req.dht.worker_sender.send(WorkerSendRequest {
            ownership: Some((work, parent_work, req.tasks.cancellation_token().clone())),
            our_tid: Some(tid),
            message,
            addr,
        });
    }
}

struct RecursiveRequestCallbacksFindNodes {}
impl RecursiveRequestCallbacks for RecursiveRequestCallbacksFindNodes {
    fn on_request_start(&self, req: &RecursiveRequest<Self>, target_node: Id20, addr: SocketAddr) {
        let mut rt = req.dht.routing_table.write();
        match rt.add_node(target_node, addr) {
            InsertResult::WasExisting | InsertResult::ReplacedBad(_) | InsertResult::Added => {
                rt.mark_outgoing_request(&target_node);
            }
            InsertResult::Ignored => {}
        }
    }

    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        _addr: SocketAddr,
        resp: &anyhow::Result<ResponseOrError>,
    ) {
        let mut table = req.dht.routing_table.write();
        if resp.is_ok() {
            table.mark_response(&target_node);
        } else {
            table.mark_error(&target_node);
        }
    }
}

struct RecursiveRequest<C: RecursiveRequestCallbacks> {
    max_depth: usize,
    useful_nodes_limit: usize,
    bootstrap_seeded: AtomicBool,
    info_hash: Id20,
    request: Request,
    dht: Arc<DhtState>,
    useful_nodes: RwLock<Vec<MaybeUsefulNode>>,
    peer_tx: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
    node_tx: tokio::sync::mpsc::UnboundedSender<(Option<Id20>, SocketAddr, usize)>,
    callbacks: C,
    tasks: OwnedTasks,
    parent_owner: Option<OwnedTasks>,
}

pub struct RequestPeersStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<SocketAddr>,
    cancel_join_handle: Option<tokio::task::JoinHandle<()>>,
    tasks: OwnedTasks,
}

impl RequestPeersStream {
    pub async fn cancel_and_wait_until(&self, deadline: tokio::time::Instant) -> bool {
        self.tasks.cancel_and_wait_until(deadline).await
    }
    fn new(
        dht: Arc<DhtState>,
        info_hash: Id20,
        announce_port: Option<u16>,
        parent_owner: Option<OwnedTasks>,
    ) -> Self {
        let (peer_tx, peer_rx) = unbounded_channel();
        let (node_tx, node_rx) = unbounded_channel();
        let tasks = dht.tasks.child();
        let rp = Arc::new(RecursiveRequest {
            tasks: tasks.clone(),
            parent_owner: parent_owner.clone(),
            max_depth: 4,
            info_hash,
            useful_nodes_limit: 256,
            bootstrap_seeded: AtomicBool::new(false),
            request: Request::GetPeers(info_hash),
            dht,
            useful_nodes: RwLock::new(Vec::new()),
            peer_tx,
            node_tx,
            callbacks: RecursiveRequestCallbacksGetPeers {
                min_distance_to_announce: Id20::from_str(
                    "0000ffffffffffffffffffffffffffffffffffff",
                )
                .unwrap(),
                announce_port,
            },
        });
        let join_handle = rp.request_peers_forever(node_rx, &tasks, parent_owner.as_ref());
        Self {
            rx: peer_rx,
            cancel_join_handle: join_handle,
            tasks,
        }
    }
}

impl Drop for RequestPeersStream {
    fn drop(&mut self) {
        self.tasks.cancel();
        if let Some(task) = &self.cancel_join_handle {
            task.abort();
        }
    }
}

impl Stream for RequestPeersStream {
    type Item = SocketAddr;

    #[inline(never)]
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl RecursiveRequest<RecursiveRequestCallbacksFindNodes> {
    async fn find_node_for_routing_table(
        dht: Arc<DhtState>,
        target: Id20,
        addrs: impl Iterator<Item = SocketAddr>,
    ) -> anyhow::Result<()> {
        let (node_tx, mut node_rx) = unbounded_channel();
        let req = RecursiveRequest {
            tasks: dht.tasks.clone(),
            parent_owner: None,
            max_depth: 4,
            info_hash: target,
            request: Request::FindNode(target),
            dht,
            useful_nodes_limit: 32,
            bootstrap_seeded: AtomicBool::new(false),
            useful_nodes: RwLock::new(Vec::new()),
            peer_tx: unbounded_channel().0,
            node_tx,
            callbacks: RecursiveRequestCallbacksFindNodes {},
        };

        let request_one = |id, addr, depth| {
            req.request_one(id, addr, depth)
                .map_err(|e| {
                    debug!("error: {e:#}");
                    e
                })
                .instrument(error_span!(
                    "find_node",
                    target = format!("{target:?}"),
                    addr = addr.to_string()
                ))
        };

        let mut futs = FuturesUnordered::new();

        let mut initial_addrs = 0;
        for addr in addrs {
            futs.push(request_one(None, addr, 0));
            initial_addrs += 1;
        }

        let mut successes = 0;
        let mut errors = 0;

        loop {
            tokio::select! {
                biased;

                r = node_rx.recv() => {
                    let (id, addr, depth) = r.unwrap();
                    futs.push(request_one(id, addr, depth))
                },
                f = futs.next() => {
                    let f = match f {
                        Some(f) => f,
                        None => {
                            // find_node recursion finished.
                            break;
                        }
                    };
                    if f.is_ok() {
                        successes += 1;
                    } else {
                        errors += 1;
                    }
                }
            }
        }
        if successes == 0 {
            bail!("no successful lookups, errors = {errors}");
        }
        debug!(
            "finished, successes = {successes}, errors = {errors}, initial_addrs = {initial_addrs}"
        );
        Ok(())
    }
}

impl RecursiveRequest<RecursiveRequestCallbacksGetPeers> {
    fn request_peers_forever(
        self: &Arc<Self>,
        mut node_rx: tokio::sync::mpsc::UnboundedReceiver<(Option<Id20>, SocketAddr, usize)>,
        tasks: &OwnedTasks,
        parent: Option<&OwnedTasks>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let this = self.clone();
        let span =
            error_span!(parent: None, "get_peers", info_hash = format!("{:?}", self.info_hash));
        let future = async move {
            let this = &this;
            // Looper adds root nodes to the queue every 60 seconds.
            let looper = {
                async move {
                    let mut iteration = 0;
                    loop {
                        trace!("iteration {}", iteration);
                        let sleep = match this.get_peers_root() {
                            Ok(0) => Duration::from_secs(1),
                            Ok(n) if n < 8 => REQUERY_INTERVAL / 8 * (n as u32),
                            Ok(_) => REQUERY_INTERVAL,
                            Err(e) => {
                                error!("error in get_peers_root(): {e:#}");
                                return Err::<(), anyhow::Error>(e);
                            }
                        };
                        tokio::time::sleep(sleep).await;
                        iteration += 1;
                    }
                }
            };
            tokio::pin!(looper);

            let mut futs = FuturesUnordered::new();
            loop {
                tokio::select! {
                    addr = node_rx.recv() => {
                        let (id, addr, depth) = addr.unwrap();
                        futs.push(
                            this.request_one(id, addr, depth)
                                .map_err(|e| debug!("error: {e:#}"))
                                .instrument(error_span!("addr", addr=addr.to_string()))
                        );
                    }
                    Some(_) = futs.next(), if !futs.is_empty() => {}
                    r = &mut looper => {
                        return r
                    }
                }
            }
        };
        // This query has two owners: the DHT root and its payload. Closing
        // either must dispose the query and fence its queued datagrams.
        let query_token = tasks.cancellation_token().clone();
        let parent_token = parent.map(|owner| owner.cancellation_token().clone());
        let future = async move {
            let _cancel_query = query_token.drop_guard();
            match parent_token {
                Some(token) => tokio::select! {
                    biased;
                    _ = token.cancelled() => Ok(()),
                    result = future => result,
                },
                None => future.await,
            }
        };
        let future: std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>,
        > = match parent {
            Some(parent) => Box::pin(parent.track_future(future)?),
            None => Box::pin(future),
        };
        tasks.spawn(span, future)
    }

    fn get_peers_root(&self) -> anyhow::Result<usize> {
        // Target the requested hash directly on cold native startup. Bootstrapping
        // the routing table around our random node ID is separate work and can
        // leave this bounded lookup traversing unrelated nodes first. These roots
        // come only from the already-vetted pinned bootstrap resolution.
        let roots = self.dht.bootstrap_roots.read();
        if !roots.is_empty() && !self.bootstrap_seeded.swap(true, Ordering::AcqRel) {
            for addr in roots.iter() {
                self.node_tx.send((None, *addr, 0))?;
            }
        }
        drop(roots);
        let mut count = 0;
        for (id, addr) in self
            .dht
            .routing_table
            .read()
            .sorted_by_distance_from(self.info_hash)
            .iter()
            .map(|n| (n.id(), n.addr()))
            .take(8)
        {
            count += 1;
            self.node_tx.send((Some(id), addr, 0))?;
        }
        Ok(count)
    }
}

impl<C: RecursiveRequestCallbacks> RecursiveRequest<C> {
    async fn request_one(
        &self,
        id: Option<Id20>,
        addr: SocketAddr,
        depth: usize,
    ) -> anyhow::Result<()> {
        if let Some(id) = id {
            self.callbacks.on_request_start(self, id, addr);
        }

        let response = self
            .dht
            .request_owned(
                &self.tasks,
                self.parent_owner.as_ref(),
                self.request.clone(),
                addr,
            )
            .await
            .inspect(|r| {
                self.mark_node_responded(addr, r);
            });
        if let Some(id) = id {
            self.callbacks.on_request_end(self, id, addr, &response);
        }

        let response = match self
            .dht
            .request_owned(
                &self.tasks,
                self.parent_owner.as_ref(),
                self.request.clone(),
                addr,
            )
            .await
        {
            Ok(ResponseOrError::Response(r)) => r,
            Ok(ResponseOrError::Error(e)) => bail!("error response: {:?}", e),
            Err(e) => {
                self.mark_node_error(addr);
                return Err(e);
            }
        };

        if let Some(peers) = response.values {
            for peer in peers {
                let peer = SocketAddr::V4(peer.addr);
                if self.dht.network_policy.allows(peer) {
                    self.peer_tx.send(peer)?;
                }
            }
        }

        if let Some(nodes) = response.nodes {
            for node in nodes.nodes {
                let addr = SocketAddr::V4(node.addr);
                let should_request = self.should_request_node(node.id, addr, depth);
                trace!(
                    "should_request={}, id={:?}, addr={}, depth={}/{}",
                    should_request,
                    node.id,
                    addr,
                    depth,
                    self.max_depth
                );
                if should_request {
                    self.node_tx.send((Some(node.id), addr, depth + 1))?;
                }
            }
        }
        Ok(())
    }

    fn mark_node_error(&self, addr: SocketAddr) -> bool {
        self.useful_nodes
            .write()
            .iter_mut()
            .find(|n| n.addr == addr)
            .map(|n| {
                n.errors_in_a_row += 1;
            })
            .is_some()
    }

    fn mark_node_responded(&self, addr: SocketAddr, response: &ResponseOrError) -> bool {
        self.useful_nodes
            .write()
            .iter_mut()
            .find(|n| n.addr == addr)
            .map(|node| {
                node.last_response = Some(Instant::now());
                node.errors_in_a_row = 0;
                match response {
                    ResponseOrError::Response(r) => {
                        node.returned_peers =
                            r.values.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
                    }
                    ResponseOrError::Error(_) => {
                        node.returned_peers = false;
                    }
                }
            })
            .is_some()
    }

    fn should_request_node(&self, node_id: Id20, addr: SocketAddr, depth: usize) -> bool {
        if !self.dht.network_policy.allows(addr) {
            return false;
        }
        if depth >= self.max_depth {
            return false;
        }

        let mut closest_nodes = self.useful_nodes.write();

        // If recently requested, ignore
        if let Some(existing) = closest_nodes.iter_mut().find(|n| n.id == node_id) {
            if existing.last_request.elapsed() > Duration::from_secs(60) {
                existing.last_request = Instant::now();
                return true;
            }
            return false;
        }

        closest_nodes.push(MaybeUsefulNode {
            id: node_id,
            addr,
            last_request: Instant::now(),
            last_response: None,
            returned_peers: false,
            errors_in_a_row: 0,
        });

        closest_nodes.sort_by_key(|n| {
            let has_returned_peers_desc = Reverse(n.returned_peers);
            let has_responded_desc = Reverse(n.last_response.is_some() as u8);
            let distance = n.id.distance(&self.info_hash);
            let freshest_response = n
                .last_response
                .map(|r| r.elapsed())
                .unwrap_or(Duration::MAX);
            (
                has_returned_peers_desc,
                has_responded_desc,
                distance,
                freshest_response,
            )
        });
        if closest_nodes.len() > self.useful_nodes_limit {
            let popped = closest_nodes.pop().unwrap();
            if popped.id == node_id {
                return false;
            }
        }
        true
    }
}

pub struct DhtState {
    id: Id20,
    next_transaction_id: AtomicU16,

    // Created requests: (transaction_id, addr) => Requests.
    // If we get a response, it gets removed from here.
    inflight_by_transaction_id: DashMap<(u16, SocketAddr), OutstandingRequest>,

    routing_table: RwLock<RoutingTable>,
    // Transient, policy-vetted pinned DNS answers; never persisted or source-supplied.
    bootstrap_roots: RwLock<Vec<SocketAddr>>,
    listen_addr: SocketAddr,

    // Sending requests to the worker.
    rate_limiter: RateLimiter,
    // This is to send raw messages
    worker_sender: UnboundedSender<WorkerSendRequest>,

    cancellation_token: CancellationToken,
    tasks: OwnedTasks,
    network_policy: NetworkPolicy,

    pub(crate) peer_store: PeerStore,
}

impl DhtState {
    fn new_internal(
        id: Id20,
        sender: UnboundedSender<WorkerSendRequest>,
        routing_table: Option<RoutingTable>,
        listen_addr: SocketAddr,
        peer_store: PeerStore,
        tasks: OwnedTasks,
        network_policy: NetworkPolicy,
    ) -> Self {
        let routing_table = routing_table.unwrap_or_else(|| RoutingTable::new(id, None));
        Self {
            id,
            next_transaction_id: AtomicU16::new(0),
            inflight_by_transaction_id: Default::default(),
            routing_table: RwLock::new(routing_table),
            bootstrap_roots: RwLock::new(Vec::new()),
            worker_sender: sender,
            listen_addr,
            rate_limiter: make_rate_limiter(),
            peer_store,
            cancellation_token: tasks.cancellation_token().clone(),
            tasks,
            network_policy,
        }
    }

    async fn request(&self, request: Request, addr: SocketAddr) -> anyhow::Result<ResponseOrError> {
        self.request_owned(&self.tasks, None, request, addr).await
    }
    async fn request_owned(
        &self,
        tasks: &OwnedTasks,
        parent: Option<&OwnedTasks>,
        request: Request,
        addr: SocketAddr,
    ) -> anyhow::Result<ResponseOrError> {
        let _request = tasks.register().context("DHT request is closing")?;
        tokio::select! {
            biased;
            _ = tasks.cancellation_token().cancelled() => bail!("DHT request is closing"),
            result = self.request_owned_inner(tasks, parent, request, addr) => result,
        }
    }
    async fn request_owned_inner(
        &self,
        tasks: &OwnedTasks,
        parent: Option<&OwnedTasks>,
        request: Request,
        addr: SocketAddr,
    ) -> anyhow::Result<ResponseOrError> {
        self.network_policy.validate_destination(addr)?;
        self.rate_limiter.acquire_one().await;
        let (tid, message) = self.create_request(request);
        let key = (tid, addr);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inflight_by_transaction_id
            .insert(key, OutstandingRequest { done: tx });
        let _slot = RequestSlot { state: self, key };
        trace!("sending {message:?}");
        let work = tasks.register().context("DHT request is closing")?;
        match self.worker_sender.send(WorkerSendRequest {
            ownership: Some((
                work,
                match parent {
                    Some(parent) => Some(parent.register().context("payload is closing")?),
                    None => None,
                },
                tasks.cancellation_token().clone(),
            )),
            our_tid: Some(tid),
            message,
            addr,
        }) {
            Ok(_) => {}
            Err(e) => {
                self.inflight_by_transaction_id.remove(&key);
                return Err(e.into());
            }
        };
        match tokio::time::timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(r)) => r.map(|r| {
                trace!("received {r:?}");
                r
            }),
            Ok(Err(e)) => {
                self.inflight_by_transaction_id.remove(&key);
                warn!("recv error, did not expect this: {:?}", e);
                Err(e.into())
            }
            Err(_) => {
                self.inflight_by_transaction_id.remove(&key);
                bail!("timeout ({RESPONSE_TIMEOUT:?})")
            }
        }
    }

    fn create_request(&self, request: Request) -> (u16, Message<ByteBufOwned>) {
        let transaction_id = self.next_transaction_id.fetch_add(1, Ordering::Relaxed);
        let transaction_id_buf = [(transaction_id >> 8) as u8, (transaction_id & 0xff) as u8];

        let message = match request {
            Request::GetPeers(info_hash) => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::GetPeersRequest(GetPeersRequest {
                    id: self.id,
                    info_hash,
                }),
            },
            Request::FindNode(target) => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::FindNodeRequest(FindNodeRequest {
                    id: self.id,
                    target,
                }),
            },
            Request::Ping => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::PingRequest(PingRequest { id: self.id }),
            },
            Request::Announce {
                info_hash,
                token,
                port,
            } => Message {
                kind: MessageKind::AnnouncePeer(AnnouncePeer {
                    id: self.id,
                    implied_port: 0,
                    info_hash,
                    port,
                    token,
                }),
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
            },
        };
        (transaction_id, message)
    }

    fn on_received_message(
        self: &Arc<Self>,
        msg: Message<ByteBufOwned>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        self.network_policy.validate_destination(addr)?;
        let generate_compact_nodes = |target| {
            let nodes = self
                .routing_table
                .read()
                .sorted_by_distance_from(target)
                .into_iter()
                .filter_map(|r| {
                    Some(Node {
                        id: r.id(),
                        addr: match r.addr() {
                            SocketAddr::V4(v4) => v4,
                            SocketAddr::V6(_) => return None,
                        },
                    })
                })
                .take(8)
                .collect::<Vec<_>>();
            CompactNodeInfo { nodes }
        };

        match &msg.kind {
            // If it's a response to a request we made, find the request task, notify it with the response,
            // and let it handle it.
            MessageKind::Error(_) | MessageKind::Response(_) => {
                let tid = msg.get_our_transaction_id().context("bad transaction id")?;
                let request = match self
                    .inflight_by_transaction_id
                    .remove(&(tid, addr))
                    .map(|(_, v)| v)
                {
                    Some(req) => req,
                    None => {
                        bail!("outstanding request not found. Message: {:?}", msg)
                    }
                };

                let response_or_error = match msg.kind {
                    MessageKind::Error(e) => ResponseOrError::Error(e),
                    MessageKind::Response(r) => ResponseOrError::Response(r),
                    _ => unreachable!(),
                };
                match request.done.send(Ok(response_or_error)) {
                    Ok(_) => {}
                    Err(e) => {
                        debug!(
                            "recieved response, but the receiver task is closed: {:?}",
                            e
                        );
                    }
                }
                return Ok(());
            }
            _ => {}
        };

        trace!("received query from {addr}: {msg:?}");

        match &msg.kind {
            // Otherwise, respond to a query.
            MessageKind::PingRequest(req) => {
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        ..Default::default()
                    }),
                };
                self.routing_table.write().mark_last_query(&req.id);
                self.worker_sender.send(WorkerSendRequest {
                    ownership: Some((
                        self.tasks.register().context("DHT is closing")?,
                        None,
                        self.tasks.cancellation_token().clone(),
                    )),
                    our_tid: None,
                    message,
                    addr,
                })?;
                Ok(())
            }
            MessageKind::AnnouncePeer(ann) => {
                self.routing_table.write().mark_last_query(&ann.id);
                let added = self.peer_store.store_peer(ann, addr);
                trace!("{addr}: added_peer={added}, announce={ann:?}");
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        ..Default::default()
                    }),
                };
                self.worker_sender.send(WorkerSendRequest {
                    ownership: Some((
                        self.tasks.register().context("DHT is closing")?,
                        None,
                        self.tasks.cancellation_token().clone(),
                    )),
                    our_tid: None,
                    message,
                    addr,
                })?;
                Ok(())
            }
            MessageKind::GetPeersRequest(req) => {
                let compact_node_info = generate_compact_nodes(req.info_hash);
                let compact_peer_info = self.peer_store.get_for_info_hash(req.info_hash);
                self.routing_table.write().mark_last_query(&req.id);
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        nodes: Some(compact_node_info),
                        values: Some(compact_peer_info),
                        token: Some(ByteBufOwned::from(
                            &self.peer_store.gen_token_for(req.id, addr)[..],
                        )),
                    }),
                };
                self.worker_sender.send(WorkerSendRequest {
                    ownership: Some((
                        self.tasks.register().context("DHT is closing")?,
                        None,
                        self.tasks.cancellation_token().clone(),
                    )),
                    our_tid: None,
                    message,
                    addr,
                })?;
                Ok(())
            }
            MessageKind::FindNodeRequest(req) => {
                let compact_node_info = generate_compact_nodes(req.target);
                self.routing_table.write().mark_last_query(&req.id);
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        nodes: Some(compact_node_info),
                        ..Default::default()
                    }),
                };
                self.worker_sender.send(WorkerSendRequest {
                    ownership: Some((
                        self.tasks.register().context("DHT is closing")?,
                        None,
                        self.tasks.cancellation_token().clone(),
                    )),
                    our_tid: None,
                    message,
                    addr,
                })?;
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    pub fn get_stats(&self) -> DhtStats {
        DhtStats {
            id: self.id,
            outstanding_requests: self.inflight_by_transaction_id.len(),
            routing_table_size: self.routing_table.read().len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Request {
    GetPeers(Id20),
    FindNode(Id20),
    Announce {
        info_hash: Id20,
        token: ByteBufOwned,
        port: u16,
    },
    Ping,
}

enum ResponseOrError {
    Response(Response<ByteBufOwned>),
    Error(ErrorDescription<ByteBufOwned>),
}

impl core::fmt::Debug for ResponseOrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(r) => write!(f, "{r:?}"),
            Self::Error(e) => write!(f, "{e:?}"),
        }
    }
}

struct DhtWorker {
    socket: UdpSocket,
    dht: Arc<DhtState>,
}

impl DhtWorker {
    fn on_send_error(&self, tid: u16, addr: SocketAddr, err: anyhow::Error) {
        if let Some((_, OutstandingRequest { done })) =
            self.dht.inflight_by_transaction_id.remove(&(tid, addr))
        {
            let _ = done.send(Err(err)).is_err();
        };
    }

    async fn bootstrap_hostname(&self, hostname: &str) -> anyhow::Result<()> {
        let addrs: Vec<_> = tokio::net::lookup_host(hostname)
            .await
            .context("Bootstrap resolution failed")?
            .collect();
        self.dht.network_policy.validate_resolution(&addrs)?;
        if self.dht.network_policy.is_native() {
            let mut roots = self.dht.bootstrap_roots.write();
            for addr in &addrs {
                if !roots.contains(addr) {
                    roots.push(*addr);
                }
            }
        }
        RecursiveRequest::find_node_for_routing_table(
            self.dht.clone(),
            self.dht.id,
            addrs.into_iter(),
        )
        .await
    }

    async fn bootstrap_hostname_with_backoff(&self, addr: &str) -> anyhow::Result<()> {
        let mut backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(10))
            .with_multiplier(1.5)
            .with_max_interval(Duration::from_secs(60))
            .with_max_elapsed_time(Some(Duration::from_secs(86400)))
            .build();

        loop {
            let backoff = match self
                .bootstrap_hostname(addr)
                .instrument(error_span!("bootstrap", hostname = addr))
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    warn!("error: {}", e);
                    backoff.next_backoff()
                }
            };
            if let Some(backoff) = backoff {
                tokio::time::sleep(backoff).await;
                continue;
            }
            bail!("bootstrap failed")
        }
    }

    async fn bootstrap(&self, bootstrap_addrs: &[String]) -> anyhow::Result<()> {
        let mut futs = FuturesUnordered::new();

        for addr in bootstrap_addrs.iter() {
            futs.push(self.bootstrap_hostname_with_backoff(addr));
        }
        let mut successes = 0;
        while let Some(resp) = futs.next().await {
            if resp.is_ok() {
                successes += 1
            }
        }
        if successes == 0 {
            bail!("bootstrapping failed")
        }
        Ok(())
    }

    async fn bucket_refresher(&self) -> anyhow::Result<()> {
        let (tx, mut rx) = unbounded_channel();

        let mut futs = FuturesUnordered::new();
        let filler = async {
            let mut interval = tokio::time::interval(INACTIVITY_TIMEOUT);
            interval.tick().await;
            let mut iteration = 0;
            loop {
                interval.tick().await;
                let mut found = 0;
                for bucket in self.dht.routing_table.read().iter_buckets() {
                    if bucket.leaf.last_refreshed.elapsed() < INACTIVITY_TIMEOUT {
                        continue;
                    }
                    found += 1;
                    let random_id = bucket.random_within();
                    tx.send(random_id).unwrap();
                }
                trace!("iteration {}, refreshing {} buckets", iteration, found);
                iteration += 1;
            }
        };

        tokio::pin!(filler);

        loop {
            tokio::select! {
                _ = &mut filler => {},
                random_id = rx.recv() => {
                    let random_id = random_id.unwrap();
                    let addrs = self
                        .dht
                        .routing_table
                        .read()
                        .sorted_by_distance_from(random_id)
                        .iter()
                        .map(|n| n.addr())
                        .take(8).collect::<Vec<_>>();
                    futs.push(
                        RecursiveRequest::find_node_for_routing_table(
                            self.dht.clone(), random_id, addrs.into_iter()
                        ).instrument(error_span!("refresh_bucket"))
                    );
                },
                _ = futs.next(), if !futs.is_empty() => {},
            }
        }
    }

    async fn pinger(&self) -> anyhow::Result<()> {
        let mut futs = FuturesUnordered::new();
        let mut interval = tokio::time::interval(INACTIVITY_TIMEOUT / 4);
        let (tx, mut rx) = unbounded_channel();
        let looper = async {
            let mut iteration = 0;
            loop {
                interval.tick().await;
                let mut found = 0;
                let now = Instant::now();
                for node in self.dht.routing_table.read().iter() {
                    if matches!(
                        node.status(now),
                        NodeStatus::Questionable | NodeStatus::Unknown
                    ) {
                        found += 1;
                        tx.send((node.id(), node.addr())).unwrap();
                    }
                }
                trace!("iteration {}, pinging {} nodes", iteration, found);
                iteration += 1;
            }
        };

        tokio::pin!(looper);

        loop {
            tokio::select! {
                _ = &mut looper => {},
                r = rx.recv() => {
                    let (id, addr) = r.unwrap();
                    futs.push(async move {
                        self.dht.routing_table.write().mark_outgoing_request(&id);
                        match self.dht.request(Request::Ping, addr).await {
                            Ok(_) => {
                                self.dht.routing_table.write().mark_response(&id);
                            },
                            Err(e) => {
                                self.dht.routing_table.write().mark_error(&id);
                                debug!("error: {e:#}");
                            }
                        }
                    }.instrument(error_span!("ping", addr=addr.to_string())))
                },
                _ = futs.next(), if !futs.is_empty() => {},
            }
        }
    }

    async fn framer(
        &self,
        socket: &UdpSocket,
        mut input_rx: UnboundedReceiver<WorkerSendRequest>,
        output_tx: Sender<(Message<ByteBufOwned>, SocketAddr)>,
    ) -> anyhow::Result<()> {
        let writer = async {
            let mut buf = Vec::new();
            while let Some(WorkerSendRequest {
                our_tid,
                message,
                addr,
                ownership,
            }) = input_rx.recv().await
            {
                if !self.dht.network_policy.allows(addr) {
                    if let Some(tid) = our_tid {
                        self.on_send_error(
                            tid,
                            addr,
                            anyhow::anyhow!("Network destination denied"),
                        );
                    }
                    continue;
                }
                if our_tid.is_none() {
                    trace!("{}: sending {:?}", addr, &message);
                }
                buf.clear();
                bprotocol::serialize_message(
                    &mut buf,
                    message.transaction_id,
                    message.version,
                    message.ip,
                    message.kind,
                )
                .unwrap();
                let sent = match ownership.as_ref() {
                    Some((_, _, token)) => tokio::select! {
                        biased;
                        _ = token.cancelled() => { continue; },
                        result = socket.send_to(&buf, addr) => result,
                    },
                    None => socket.send_to(&buf, addr).await,
                };
                if let Err(e) = sent {
                    debug!("error sending to {addr}: {e:#}");
                    if let Some(tid) = our_tid {
                        self.on_send_error(tid, addr, e.into());
                    }
                }
            }
            Err::<(), _>(anyhow::anyhow!(
                "DHT UDP socket writer over, nowhere to read messages from"
            ))
        };
        let reader = async {
            let mut buf = vec![0u8; 16384];
            loop {
                let (size, addr) = socket
                    .recv_from(&mut buf)
                    .await
                    .context("error reading from UDP socket")?;
                if !self.dht.network_policy.allows(addr) {
                    continue;
                }
                match bprotocol::deserialize_message::<ByteBufOwned>(&buf[..size]) {
                    Ok(msg) => match output_tx.send((msg, addr)).await {
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Err(e) => debug!("{}: error deserializing incoming message: {}", addr, e),
                }
            }
            Err::<(), _>(anyhow::anyhow!(
                "DHT UDP socket reader over, nowhere to send responses to"
            ))
        };
        let result = tokio::select! {
            err = writer => err,
            err = reader => err,
        };
        result.context("DHT UDP framer closed")
    }

    async fn start(
        self,
        in_rx: UnboundedReceiver<WorkerSendRequest>,
        bootstrap_addrs: &[String],
    ) -> anyhow::Result<()> {
        let (out_tx, mut out_rx) = channel(1);
        let framer = self
            .framer(&self.socket, in_rx, out_tx)
            .instrument(debug_span!("dht_framer"));

        let bootstrap = self.bootstrap(bootstrap_addrs);
        let mut bootstrap_done = false;

        let response_reader = {
            let this = &self;
            async move {
                while let Some((response, addr)) = out_rx.recv().await {
                    if let Err(e) = this.dht.on_received_message(response, addr) {
                        debug!("error in on_response, addr={:?}: {}", addr, e)
                    }
                }
                Err::<(), _>(anyhow::anyhow!(
                    "closed response reader, nowhere to send results to, DHT closed"
                ))
            }
        }
        .instrument(debug_span!("dht_responese_reader"));

        let pinger = self.pinger().instrument(error_span!("pinger"));
        let bucket_refresher = self
            .bucket_refresher()
            .instrument(error_span!("bucket_refresher"));

        tokio::pin!(framer);
        tokio::pin!(bootstrap);
        tokio::pin!(response_reader);
        tokio::pin!(pinger);
        tokio::pin!(bucket_refresher);

        loop {
            tokio::select! {
                err = &mut framer => {
                    anyhow::bail!("framer quit: {:?}", err)
                },
                result = &mut bootstrap, if !bootstrap_done => {
                    bootstrap_done = true;
                    result?;
                },
                err = &mut pinger => {
                    anyhow::bail!("pinger quit: {:?}", err)
                },
                err = &mut bucket_refresher => {
                    anyhow::bail!("bucket_refresher quit: {:?}", err)
                },
                err = &mut response_reader => {anyhow::bail!("response reader quit: {:?}", err)}
            }
        }
    }
}

#[derive(Default)]
pub struct DhtConfig {
    pub network_policy: NetworkPolicy,
    pub peer_id: Option<Id20>,
    pub bootstrap_addrs: Option<Vec<String>>,
    pub routing_table: Option<RoutingTable>,
    pub listen_addr: Option<SocketAddr>,
    pub peer_store: Option<PeerStore>,
    pub cancellation_token: Option<CancellationToken>,
    pub task_owner: Option<OwnedTasks>,
}

impl DhtState {
    pub async fn new() -> anyhow::Result<Arc<Self>> {
        Self::with_config(DhtConfig::default()).await
    }
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    #[inline(never)]
    pub fn with_config(mut config: DhtConfig) -> BoxFuture<'static, anyhow::Result<Arc<Self>>> {
        async move {
            if config.network_policy.is_native() {
                if !config.network_policy.allows_dht()
                    || config.bootstrap_addrs.is_some()
                    || config.routing_table.is_some()
                    || config.peer_store.is_some()
                {
                    bail!("Native DHT configuration denied");
                }
            }
            let tasks = match config.task_owner.take() {
                Some(owner) => {
                    if config.cancellation_token.is_some() {
                        bail!("task owner and cancellation token cannot both be set");
                    }
                    owner.child()
                }
                None => OwnedTasks::new(config.cancellation_token.take().unwrap_or_default()),
            };
            let _construction = tasks.register().context("DHT scope is closing")?;

            let socket = match config.listen_addr {
                Some(addr) => UdpSocket::bind(addr)
                    .await
                    .with_context(|| format!("error binding socket, address {addr}")),
                None => UdpSocket::bind("0.0.0.0:0")
                    .await
                    .context("error binding socket, address 0.0.0.0:0"),
            }?;

            let listen_addr = socket
                .local_addr()
                .context("cannot determine UDP listen addr")?;
            info!("DHT listening on {:?}", listen_addr);

            let peer_id = config
                .peer_id
                .unwrap_or_else(|| (generate_azereus_style(*b"rQ", crate_version!())));
            info!("starting up DHT with peer id {:?}", peer_id);
            let bootstrap_addrs = if config.network_policy.is_native() {
                PUBLIC_BOOTSTRAP.iter().map(|s| s.to_string()).collect()
            } else {
                config
                    .bootstrap_addrs
                    .unwrap_or_else(|| crate::DHT_BOOTSTRAP.iter().map(|v| v.to_string()).collect())
            };

            let (in_tx, in_rx) = unbounded_channel();
            let state = Arc::new(Self::new_internal(
                peer_id,
                in_tx,
                config.routing_table,
                listen_addr,
                config.peer_store.unwrap_or_else(|| PeerStore::new(peer_id)),
                tasks.clone(),
                config.network_policy,
            ));

            let _ = state.tasks.spawn(error_span!("dht"), {
                let state = state.clone();
                async move {
                    let worker = DhtWorker { socket, dht: state };
                    worker.start(in_rx, &bootstrap_addrs).await
                }
            });
            Ok(state)
        }
        .boxed()
    }

    pub fn tasks(&self) -> &OwnedTasks {
        &self.tasks
    }
    pub async fn shutdown_and_wait_until(&self, deadline: tokio::time::Instant) -> bool {
        self.tasks.cancel_and_wait_until(deadline).await
    }

    pub fn get_peers(
        self: &Arc<Self>,
        info_hash: Id20,
        announce_port: Option<u16>,
    ) -> RequestPeersStream {
        RequestPeersStream::new(self.clone(), info_hash, announce_port, None)
    }

    /// Lookup work and queued datagrams also retain the supplied payload owner.
    pub fn get_peers_owned(
        self: &Arc<Self>,
        info_hash: Id20,
        announce_port: Option<u16>,
        owner: OwnedTasks,
    ) -> RequestPeersStream {
        RequestPeersStream::new(self.clone(), info_hash, announce_port, Some(owner))
    }
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn stats(&self) -> DhtStats {
        self.get_stats()
    }

    pub fn with_routing_table<R, F: FnOnce(&RoutingTable) -> R>(&self, f: F) -> R {
        f(&self.routing_table.read())
    }

    pub fn clone_routing_table(&self) -> RoutingTable {
        self.routing_table.read().clone()
    }
}

#[cfg(test)]
mod native_policy_tests {
    use super::*;
    fn isolated_state(sender: UnboundedSender<WorkerSendRequest>) -> Arc<DhtState> {
        let id = Id20::new([0; 20]);
        Arc::new(DhtState::new_internal(
            id,
            sender,
            None,
            "127.0.0.1:1".parse().unwrap(),
            PeerStore::new(id),
            OwnedTasks::new(CancellationToken::new()),
            NetworkPolicy::PublicDhtTcpV1,
        ))
    }
    #[tokio::test]
    async fn native_private_request_and_routing_node_are_refused_before_enqueue() {
        let (sender, mut receiver) = unbounded_channel();
        let state = isolated_state(sender);
        let denied = "127.0.0.1:6881".parse().unwrap();
        assert!(state.request(Request::Ping, denied).await.is_err());
        assert!(receiver.try_recv().is_err());
        let request = RecursiveRequest {
            tasks: state.tasks.clone(),
            parent_owner: None,
            max_depth: 4,
            info_hash: state.id,
            request: Request::FindNode(state.id),
            dht: state.clone(),
            useful_nodes_limit: 32,
            bootstrap_seeded: AtomicBool::new(false),
            useful_nodes: RwLock::new(Vec::new()),
            peer_tx: unbounded_channel().0,
            node_tx: unbounded_channel().0,
            callbacks: RecursiveRequestCallbacksFindNodes {},
        };
        assert!(!request.should_request_node(Id20::new([1; 20]), denied, 0));
        assert!(request.useful_nodes.read().is_empty());
        state.tasks.cancel();
    }
    #[test]
    fn cold_native_peer_lookup_uses_vetted_bootstrap_before_routing_table() {
        let (sender, _) = unbounded_channel();
        let state = isolated_state(sender);
        let bootstrap: SocketAddr = "1.1.1.1:6881".parse().unwrap();
        state
            .network_policy
            .validate_resolution(&[bootstrap])
            .unwrap();
        state.bootstrap_roots.write().push(bootstrap);
        let (node_tx, mut node_rx) = unbounded_channel();
        let request = RecursiveRequest {
            tasks: state.tasks.clone(),
            parent_owner: None,
            max_depth: 4,
            info_hash: Id20::new([1; 20]),
            request: Request::GetPeers(Id20::new([1; 20])),
            dht: state.clone(),
            useful_nodes_limit: 256,
            bootstrap_seeded: AtomicBool::new(false),
            useful_nodes: RwLock::new(Vec::new()),
            peer_tx: unbounded_channel().0,
            node_tx,
            callbacks: RecursiveRequestCallbacksGetPeers {
                min_distance_to_announce: Id20::new([0; 20]),
                announce_port: None,
            },
        };
        assert_eq!(request.get_peers_root().unwrap(), 0);
        assert_eq!(node_rx.try_recv().unwrap(), (None, bootstrap, 0));
        request.get_peers_root().unwrap();
        assert!(
            node_rx.try_recv().is_err(),
            "Pinned roots must not be polled repeatedly"
        );
        state.tasks.cancel();
    }

    #[tokio::test]
    async fn native_final_datagram_boundary_denies_bypass_contact() {
        let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (sender, receiver) = unbounded_channel();
        let state = isolated_state(sender.clone());
        let (_, message) = state.create_request(Request::Ping);
        sender
            .send(WorkerSendRequest {
                our_tid: None,
                message,
                addr: target.local_addr().unwrap(),
                ownership: None,
            })
            .unwrap();
        // Drop the state-held sender by closing receiver admission through a bounded select.
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let worker = DhtWorker {
            socket,
            dht: state.clone(),
        };
        let (output, _) = channel(1);
        let _ = tokio::time::timeout(
            Duration::from_millis(30),
            worker.framer(&worker.socket, receiver, output),
        )
        .await;
        let mut buffer = [0; 1024];
        assert!(
            tokio::time::timeout(Duration::from_millis(30), target.recv_from(&mut buffer))
                .await
                .is_err()
        );
        state.tasks.cancel();
    }
    #[tokio::test]
    async fn native_bootstrap_override_is_denied_before_udp_bind() {
        assert!(DhtState::with_config(DhtConfig {
            network_policy: NetworkPolicy::PublicDhtTcpV1,
            bootstrap_addrs: Some(vec!["127.0.0.1:1".into()]),
            ..Default::default()
        })
        .await
        .is_err());
    }
}
