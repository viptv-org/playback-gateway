# Maintained rqbit dependency sources

These are canonical Apache-2.0 upstream sources, adopted from the existing
gateway working tree. `UPSTREAM.json` records exact upstream revisions,
original source hashes, original canonical manifest hashes and registry
checksums. The root gateway commit pins the maintained snapshot. Original
manifests are retained in `upstream-manifests`; the active canonical manifests
replace unavailable sibling paths with the same registry versions, except
these four maintained packages. Root `Cargo.lock` is authoritative.

Local modifications add `librqbit-core::owned_tasks::OwnedTasks`: registration
before execution/enqueue; admission fences during closure; parent/child
cancellation and counting; receipt disposal after worker/resource destruction.
Blocking IO stays charged while its actual closure/poll executes. A registered completion waiter
keeps every task charged until Tokio reports the entire task finished. Inner
future disposal alone cannot acknowledge task-held sockets as closed. Queued disk
writes and DHT datagrams retain their payload before enqueue and carry ownership
into their shared consumer. Payload
cancellation also cancels its DHT query, while another payload retains the root.
Initialization and live integrity checks observe cancellation between 64 KiB
hash/read chunks and file operations. Storage creation and synchronous work are
covered by admission/worker receipts. Native file reads register their storage
poll and wake interrupted piece waits on payload cancellation.

Joined APIs use one caller-supplied absolute Tokio deadline:

- `OwnedTasks::cancel_and_settle_until` returns `Settlement::Settled` only for
  an empty, fenced group; a timeout returns static `SettlementFailed`.
- `ManagedTorrent::stop_and_settle_until` retires initialization and live work.
  `stop_and_wait_until` remains its boolean compatibility wrapper.
- `Session::shutdown_and_settle_until` joins session, DHT root and auxiliary
  UDP tracker tasks. Metadata admission observes session cancellation.
- DHT root/query and UDP tracker expose `shutdown_and_wait_until` /
  `cancel_and_wait_until`. Query cancellation joins its queued datagrams.

Failures latch on all ancestor owners. `has_failed_settlement` stays true after
delayed work finally exits. The gateway retains payload accounting/cache locks
while work is pending, joins before deletion, and refuses new independent
selected-file admission after failed settlement for that manager lifetime.
Legacy `Session::stop` is a cancellation signal with a historical delay and
does **not** certify joined completion. Consumers must use the explicit APIs.
The manager's destructor also signals cancellation; embedders must retain the
manager/cache owner through failed settlement until all explicit receipts
settle. The selected acquisition layer must supply that ownership when callers
drop an acquisition before a handle exists.

Optional session persistence/foreign drivers are not qualified: joined session
shutdown refuses success with configured session persistence. Native use must
disable persistence, UPnP, trackers and watch features; network policy is an
independent gate. Externally borrowed streams are not ownership of their entire
consumer future: callers must separately join listeners/bodies and retain their
reservation during IO. The selected acquisition/FFI layer owns those receipts.

An arbitrary stalled synchronous OS syscall cannot safely be forced to exit in
process. The controlled filesystem regression holds IO past the two-second
deadline, requires failure plus retained accounting, then proves actual joined
completion before deletion on release. This is containment evidence, not an
unconditional two-second syscall guarantee. Native capability remains disabled
until all applicable qualification gates pass.

Run the owning workspace checks, the patched core ownership tests and focused
real dependency lifetime regressions (with the owner's worker limit):

```sh
CARGO_BUILD_JOBS=1 cargo test --locked --workspace
CARGO_BUILD_JOBS=1 cargo test --locked -p playback-engine --test torrent_ownership
CARGO_BUILD_JOBS=1 cargo test --locked -p playback-engine torrent::lifetime_tests
```
