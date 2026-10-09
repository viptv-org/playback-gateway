// SPDX-License-Identifier: GPL-2.0-only
// Package torrentruntime is the qualification candidate for a shared byte
// transport. Production consumers must not activate it before QUALIFICATION.md
// records the comparative and platform gates as passed.
package torrentruntime

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/iplist"
	"github.com/anacrolix/torrent/metainfo"
)

type Config struct {
	CacheDir       string `json:"cache_dir"`
	CacheBytes     int64  `json:"cache_bytes"`
	ReadaheadBytes int64  `json:"readahead_bytes"`
	MaxInputs      int    `json:"max_inputs,omitempty"`
	MaxHandles     int    `json:"max_handles,omitempty"`
	clientTuning   func(*torrent.ClientConfig)
	primeHeaders   bool
	utpDialDelay   time.Duration
}
type Source struct {
	Magnet       string   `json:"magnet,omitempty"`
	Metainfo     []byte   `json:"metainfo,omitempty"`
	FileIndex    *int     `json:"file_index,omitempty"`
	Trackers     []string `json:"trackers,omitempty"`
	ArchiveIndex *int     `json:"archive_index,omitempty"`
}

func (Source) String() string   { return "[private torrent source]" }
func (Source) GoString() string { return "[private torrent source]" }

type Progress struct {
	Stage        string `json:"stage"`
	Ready        bool   `json:"ready"`
	Error        string `json:"error,omitempty"`
	FileIndex    int    `json:"file_index"`
	ArchiveIndex *int   `json:"archive_index,omitempty"`
	Length       int64  `json:"length"`
}

type Runtime struct {
	mu               sync.Mutex
	client           *torrent.Client
	cache            *Cache
	server           *http.Server
	listener         net.Listener
	handles          map[string]*Handle
	retiring         map[*Handle]chan struct{}
	shutdownDone     chan struct{}
	shutdownError    error
	refs             map[*torrent.Torrent]int
	demands          map[*demandReader]pieceWindow
	priorities       map[*torrent.Torrent]map[int]torrent.PiecePriority
	readAhead        int64
	closed           bool
	privateFixture   bool
	metadataProgress sync.Map
	requests         requestObservations
	primeHeaders     bool
	maxInputs        int
	maxHandles       int
}

type Handle struct {
	mu           sync.Mutex
	r            *Runtime
	t            *torrent.Torrent
	file         *torrent.File
	openMedia    func(context.Context) (io.ReadSeekCloser, error)
	token        string
	ctx          context.Context
	cancel       context.CancelFunc
	done         chan struct{}
	once         sync.Once
	progress     Progress
	expires      time.Time
	started      time.Time
	progressAt   time.Time
	firstFrame   bool
	connected    bool
	startedBoot  int64
	progressBoot int64
	expiresBoot  int64
	primeCancel  context.CancelFunc
	consumed     []byteSpan
}

func New(cfg Config) (*Runtime, error) { return newRuntime(cfg, false) }
func newRuntime(cfg Config, privateFixture bool) (*Runtime, error) {
	if _, err := elapsedNow(); err != nil {
		return nil, errors.New("suspension-aware clock unavailable")
	}
	if cfg.CacheBytes == 0 {
		cfg.CacheBytes = 2 << 30
	}
	if cfg.MaxInputs == 0 {
		cfg.MaxInputs = 2
	}
	if cfg.MaxHandles == 0 {
		cfg.MaxHandles = 32
	}
	if cfg.MaxInputs < 1 || cfg.MaxInputs > 8 || cfg.MaxHandles < 1 || cfg.MaxHandles > 64 {
		return nil, errors.New("invalid runtime limits")
	}
	c, err := OpenCache(cfg.CacheDir, cfg.CacheBytes)
	if err != nil {
		return nil, err
	}
	readAhead := cfg.ReadaheadBytes
	if readAhead <= 0 {
		readAhead = 32 << 20
	}
	readAhead = min(readAhead, c.limit/4)
	r := &Runtime{cache: c, handles: map[string]*Handle{}, retiring: map[*Handle]chan struct{}{}, shutdownDone: make(chan struct{}), refs: map[*torrent.Torrent]int{}, demands: map[*demandReader]pieceWindow{}, priorities: map[*torrent.Torrent]map[int]torrent.PiecePriority{}, readAhead: readAhead, privateFixture: privateFixture}
	r.primeHeaders = cfg.primeHeaders
	r.maxInputs = cfg.MaxInputs
	r.maxHandles = cfg.MaxHandles
	cc := torrent.NewDefaultClientConfig()
	cc.DefaultStorage = c
	cc.ListenPort = 0
	cc.NoDefaultPortForwarding = true
	cc.Seed = false
	cc.DisableWebtorrent = true
	cc.DisableWebseeds = true
	cc.Slogger = slog.New(slog.NewTextHandler(io.Discard, nil))
	cc.IPBlocklist = publicPeers{allowPrivate: privateFixture}
	cc.TrackerDialContext = publicDial(privateFixture)
	cc.TrackerListenPacket = func(network, address string) (net.PacketConn, error) {
		pc, err := net.ListenPacket(network, address)
		if err != nil {
			return nil, err
		}
		return &publicPacket{PacketConn: pc, allowPrivate: privateFixture}, nil
	}
	cc.HTTPDialContext = publicDial(privateFixture)
	cc.Callbacks.PeerConnReadExtensionMessage = []func(torrent.PeerConnReadExtensionMessageEvent){r.metadataMessage}
	cc.Callbacks.CompletedHandshake = r.handshake
	cc.Callbacks.StatusUpdated = []func(torrent.StatusUpdatedEvent){r.trackerEvent}
	cc.Callbacks.SentRequest = []func(torrent.PeerRequestEvent){r.requests.sent}
	cc.Callbacks.DeletedRequest = []func(torrent.PeerRequestEvent){r.requests.deleted}
	cc.Callbacks.ReceivedUsefulData = []func(torrent.PeerMessageEvent){r.requests.received}
	cc.Callbacks.PeerClosed = []func(*torrent.Peer){r.requests.closed}
	if privateFixture {
		cc.NoDHT = true
		cc.ListenHost = func(string) string { return "127.0.0.1" }
		cc.DisableIPv6 = true
	}
	if cfg.clientTuning != nil {
		cfg.clientTuning(cc)
	}
	if cfg.utpDialDelay > 0 {
		cc.DialForPeerConns = false
	}
	client, err := torrent.NewClient(cc)
	if err != nil {
		c.Close()
		return nil, errors.New("torrent engine unavailable")
	}
	if cfg.utpDialDelay > 0 {
		for _, listener := range client.Listeners() {
			dialer, ok := listener.(torrent.Dialer)
			if !ok {
				client.Close()
				c.Close()
				return nil, errors.New("peer dialer unavailable")
			}
			if strings.HasPrefix(dialer.DialerNetwork(), "udp") {
				dialer = delayedDialer{dialer, cfg.utpDialDelay, client.Closed()}
			}
			client.AddDialer(dialer)
		}
	}
	l, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		client.Close()
		c.Close()
		return nil, errors.New("local reader unavailable")
	}
	r.client = client
	c.beforeEvict = r.invalidateBeforeEviction
	c.onFailure = func() {
		r.mu.Lock()
		handles := make([]*Handle, 0, len(r.handles))
		for _, h := range r.handles {
			handles = append(handles, h)
		}
		r.mu.Unlock()
		for _, h := range handles {
			h.fail("cache_unavailable")
		}
	}
	r.listener = l
	r.server = &http.Server{Handler: http.HandlerFunc(r.serve), ReadHeaderTimeout: 5 * time.Second, IdleTimeout: 30 * time.Second, MaxHeaderBytes: 8192}
	r.restoreRouting()
	go r.server.Serve(l)
	return r, nil
}

func (r *Runtime) invalidateBeforeEviction(key string) {
	var hash metainfo.Hash
	decoded, err := hex.DecodeString(key[:40])
	if err != nil {
		return
	}
	copy(hash[:], decoded)
	index, err := strconv.ParseUint(key[41:], 16, 32)
	if err != nil {
		return
	}
	for {
		// Storage has released its mutex. Fence the entire engine lookup/update
		// against Drop: a retained dropped torrent cannot safely update the
		// shared upstream request-order object.
		r.mu.Lock()
		t, ok := r.client.Torrent(hash)
		if !ok || t.Info() == nil || int(index) >= t.NumPieces() {
			r.mu.Unlock()
			return
		}
		piece := t.Piece(int(index))
		state := piece.State()
		if !state.Marking && !state.Hashing && !state.QueuedForHash {
			piece.UpdateCompletion()
			r.mu.Unlock()
			return
		}
		// MarkComplete returns before upstream publishes its completion bit.
		// Wait for that publication before revoking it, otherwise its later true
		// bit could overwrite our invalidation. The tombstone excludes new IO;
		// readers and allocations remain fenced, and Drop remains free to run.
		r.mu.Unlock()
		time.Sleep(time.Millisecond)
	}
}

func parseSource(source Source) (*torrent.TorrentSpec, error) {
	if (source.Magnet == "") == (len(source.Metainfo) == 0) || len(source.Metainfo) > 4<<20 || len(source.Magnet) > 16384 || len(source.Trackers) > 32 {
		return nil, errors.New("invalid torrent source")
	}
	var spec *torrent.TorrentSpec
	var err error
	if source.Magnet != "" {
		spec, err = torrent.TorrentSpecFromMagnetUri(source.Magnet)
	} else {
		var mi *metainfo.MetaInfo
		mi, err = metainfo.Load(bytes.NewReader(source.Metainfo))
		if err == nil {
			spec, err = torrent.TorrentSpecFromMetaInfoErr(mi)
		}
	}
	if err != nil {
		return nil, errors.New("invalid torrent metadata")
	}
	if len(spec.InfoBytes) > 0 {
		var info metainfo.Info
		if bencode.Unmarshal(spec.InfoBytes, &info) != nil {
			return nil, errors.New("invalid torrent metadata")
		}
		if info.Private != nil && *info.Private {
			return nil, errors.New("private torrent unsupported")
		}
	}
	// The first qualification slice deliberately accepts v1 metainfo only.
	// Reject an unsupported identity rather than keying multiple v2 torrents by zero.
	if spec.InfoHash == (metainfo.Hash{}) || spec.InfoHashV2.Ok {
		return nil, errors.New("unsupported torrent identity")
	}
	if len(spec.Webseeds) > 0 || len(spec.Sources) > 0 || len(spec.PeerAddrs) > 0 || len(spec.DhtNodes) > 0 {
		return nil, errors.New("unsupported source discovery field")
	}
	spec.Trackers = append(spec.Trackers, source.Trackers)
	count := 0
	for _, tier := range spec.Trackers {
		for _, tracker := range tier {
			count++
			u, e := url.Parse(tracker)
			if count > 32 || len(tracker) > 2048 || e != nil || u.Hostname() == "" || u.User != nil || (u.Scheme != "http" && u.Scheme != "https" && u.Scheme != "udp") {
				return nil, errors.New("invalid tracker")
			}
		}
	}
	return spec, nil
}

func (r *Runtime) Prepare(parent context.Context, source Source, expires time.Time) (*Handle, error) {
	started := time.Now()
	boot, clockError := elapsedNow()
	if clockError != nil {
		return nil, errors.New("clock unavailable")
	}
	if !expires.After(time.Now()) || expires.After(time.Now().Add(time.Minute)) {
		return nil, errors.New("invalid authority deadline")
	}
	spec, err := parseSource(source)
	if err != nil {
		return nil, err
	}
	if source.FileIndex != nil && (*source.FileIndex < 0 || *source.FileIndex > 65535) {
		return nil, errors.New("invalid file index")
	}
	trackerCount := 0
	for _, tier := range spec.Trackers {
		trackerCount += len(tier)
	}
	if trackerCount == 0 && !r.privateFixture {
		spec.Trackers = [][]string{append([]string(nil), defaultTrackers...)}
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.closed {
		return nil, errors.New("runtime closed")
	}
	r.cache.mu.Lock()
	cacheFailed := r.cache.failed
	r.cache.mu.Unlock()
	if cacheFailed {
		return nil, ErrCacheUnavailable
	}
	if len(r.handles)+len(r.retiring) >= r.maxHandles {
		return nil, errors.New("handle capacity unavailable")
	}
	inputs := map[*torrent.Torrent]bool{}
	for t := range r.refs {
		inputs[t] = true
	}
	for h := range r.retiring {
		inputs[h.t] = true
	}
	if _, exists := r.client.Torrent(spec.InfoHash); !exists && len(inputs) >= r.maxInputs {
		return nil, errors.New("input capacity unavailable")
	}
	if len(spec.InfoBytes) == 0 {
		spec.InfoBytes = r.restoreMetainfo(spec.InfoHash.HexString())
	}
	t, _, err := r.client.AddTorrentSpec(spec)
	if err != nil {
		return nil, errors.New("torrent admission failed")
	}
	token := make([]byte, 32)
	if _, err = rand.Read(token); err != nil {
		if r.refs[t] == 0 {
			t.Drop()
		}
		return nil, errors.New("local capability unavailable")
	}
	ctx, cancel := context.WithCancel(parent)
	h := &Handle{r: r, t: t, token: hex.EncodeToString(token), ctx: ctx, cancel: cancel, done: make(chan struct{}), expires: expires, started: started, startedBoot: boot, progressBoot: boot, expiresBoot: boot + int64(expires.Sub(started)), progress: Progress{Stage: "finding_peers", FileIndex: -1}}
	h.progressAt = h.started
	r.refs[t]++
	r.handles[h.token] = h
	var index *int
	if source.FileIndex != nil {
		v := *source.FileIndex
		index = &v
	}
	var archiveIndex *int
	if source.ArchiveIndex != nil {
		v := *source.ArchiveIndex
		archiveIndex = &v
	}
	go h.prepare(index, archiveIndex)
	go h.watch()
	return h, nil
}

func (h *Handle) prepare(index, archiveIndex *int) {
	defer close(h.done)
	select {
	case <-h.ctx.Done():
		return
	case <-h.t.GotInfo():
	}
	info := h.t.Info()
	if info.Private != nil && *info.Private {
		h.fail("private_torrent_unsupported")
		return
	}
	if err := h.saveMetainfo(); err != nil {
		h.fail("metadata_cache_unavailable")
		return
	}
	files := h.t.Files()
	if len(files) == 0 || len(files) > 4096 {
		h.fail("invalid_metadata")
		return
	}
	selected := 0
	if index != nil {
		selected = *index
		if selected >= len(files) {
			h.fail("file_index_unavailable")
			return
		}
	} else {
		for i, f := range files {
			if f.Length() > files[selected].Length() {
				selected = i
			}
		}
	}
	f := files[selected]
	if f.Length() <= 0 {
		h.fail("empty_file")
		return
	}
	length := f.Length()
	var resolvedArchiveIndex *int
	open := func(ctx context.Context) (io.ReadSeekCloser, error) {
		return &demandReader{h: h, ctx: ctx, file: f}, nil
	}
	if archiveName(f.Path()) {
		h.mu.Lock()
		h.progress.Stage = "opening_archive"
		h.mu.Unlock()
		catalog, err := h.storedArchive(h.ctx, f, archiveIndex)
		if err != nil {
			h.fail(err.Error())
			return
		}
		length = catalog.length
		resolved := catalog.index
		resolvedArchiveIndex = &resolved
		open = func(ctx context.Context) (io.ReadSeekCloser, error) {
			return h.openStoredArchive(ctx, f, catalog.index)
		}
	} else if archiveIndex != nil {
		h.fail("invalid_archive_selection")
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.ctx.Err() != nil {
		return
	}
	h.file = f
	h.openMedia = open
	h.progress = Progress{Stage: "buffering", Ready: true, FileIndex: selected, ArchiveIndex: resolvedArchiveIndex, Length: length}
	if h.r.primeHeaders && !archiveName(f.Path()) {
		ctx, cancel := context.WithCancel(h.ctx)
		h.primeCancel = cancel
		for _, offset := range []int64{0, max(0, f.Length()-1)} {
			go func(offset int64) {
				reader := &demandReader{h: h, ctx: ctx, file: f, readAheadOverride: 1}
				defer reader.Close()
				if _, err := reader.Seek(offset, io.SeekStart); err == nil {
					var b [1]byte
					_, _ = reader.Read(b[:])
				}
			}(offset)
		}
	}
}

func (h *Handle) watch() {
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-h.ctx.Done():
			h.Close()
			return
		case now := <-ticker.C:
			boot, err := elapsedNow()
			if err != nil {
				h.fail("clock_unavailable")
				return
			}
			stats := h.t.Stats()
			metadataAt := h.r.metadataProgressAt(h.t)
			h.mu.Lock()
			expired := !now.Before(h.expires) || boot >= h.expiresBoot
			if stats.ActivePeers > 0 && !h.connected {
				h.progressAt = now
				h.progressBoot = boot
			}
			if metadataAt.After(h.progressAt) {
				h.progressAt = metadataAt
				h.progressBoot = boot
			}
			if stats.ActivePeers > 0 && h.progress.Stage == "finding_peers" {
				h.progress.Stage = "fetching_metadata"
			}
			h.connected = h.connected || stats.ActivePeers > 0
			started, progressAt, firstFrame := h.started, h.progressAt, h.firstFrame
			startedBoot, progressBoot := h.startedBoot, h.progressBoot
			h.mu.Unlock()
			if expired {
				h.fail("authority_expired")
				return
			}
			if !firstFrame && (now.Sub(started) >= 120*time.Second || boot-startedBoot >= int64(120*time.Second)) {
				h.fail("startup_deadline")
				return
			}
			if !firstFrame && (now.Sub(progressAt) >= 60*time.Second || boot-progressBoot >= int64(60*time.Second)) {
				h.fail("startup_stalled")
				return
			}
		}
	}
}

// Decoder acknowledgement is distinct from availability of a local endpoint.
func (h *Handle) ReportFirstFrame() error {
	h.mu.Lock()
	defer h.mu.Unlock()
	boot, err := elapsedNow()
	if err != nil || boot >= h.expiresBoot || boot-h.startedBoot >= int64(120*time.Second) || !h.progress.Ready || h.ctx.Err() != nil || !h.expires.After(time.Now()) {
		return errors.New("first frame refused")
	}
	h.firstFrame = true
	h.consumed = nil
	if h.primeCancel != nil {
		h.primeCancel()
	}
	h.progress.Stage = "playing"
	return nil
}
func (h *Handle) fail(code string) {
	h.mu.Lock()
	h.progress.Error = code
	h.progress.Ready = false
	h.mu.Unlock()
	h.Close()
}
func (h *Handle) Observe() Progress {
	h.mu.Lock()
	defer h.mu.Unlock()
	progress := h.progress
	if progress.ArchiveIndex != nil {
		index := *progress.ArchiveIndex
		progress.ArchiveIndex = &index
	}
	return progress
}
func (h *Handle) MediaURL() string {
	return "http://" + h.r.listener.Addr().String() + "/media/" + h.token
}
func (h *Handle) Renew(expires time.Time) error {
	h.mu.Lock()
	defer h.mu.Unlock()
	now := time.Now()
	boot, err := elapsedNow()
	if err != nil || boot >= h.expiresBoot || h.ctx.Err() != nil || !h.expires.After(now) || !expires.After(now) || expires.After(now.Add(time.Minute)) {
		return errors.New("authority renewal refused")
	}
	h.expires = expires
	h.expiresBoot = boot + int64(expires.Sub(now))
	return nil
}
func (h *Handle) Close() {
	h.once.Do(func() {
		h.cancel()
		r := h.r
		r.mu.Lock()
		joined := make(chan struct{})
		r.retiring[h] = joined
		defer func() {
			r.mu.Lock()
			delete(r.retiring, h)
			r.mu.Unlock()
			close(joined)
		}()
		delete(r.handles, h.token)
		r.refs[h.t]--
		last := r.refs[h.t] == 0
		if last {
			delete(r.refs, h.t)
			delete(r.priorities, h.t)
			h.t.Drop()
		}
		r.mu.Unlock()
		if last {
			// Upstream Drop joins storage, but dial/handshake attempts are still
			// counted until outgoingConnection retires them. Do not report joined
			// transport cleanup early. The private Controller bounds this wait to
			// two seconds and orders termination of its owned worker if necessary.
			for h.t.Stats().HalfOpenPeers != 0 {
				time.Sleep(time.Millisecond)
			}
			r.mu.Lock()
			// A late completed-handshake callback can still update the meter
			// during joined retirement. Remove it afterward, while preserving a
			// new authorized playback of the same hash admitted meanwhile.
			if _, live := r.client.Torrent(h.t.InfoHash()); !live {
				r.metadataProgress.Delete(h.t.InfoHash())
			}
			r.mu.Unlock()
		}
	})
}

func (r *Runtime) serve(w http.ResponseWriter, req *http.Request) {
	if req.Method != "GET" && req.Method != "HEAD" {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	token := strings.TrimPrefix(req.URL.Path, "/media/")
	r.mu.Lock()
	h := r.handles[token]
	r.mu.Unlock()
	if h == nil || req.URL.Path != "/media/"+token {
		http.NotFound(w, req)
		return
	}
	h.mu.Lock()
	open := h.openMedia
	boot, clockErr := elapsedNow()
	valid := clockErr == nil && boot < h.expiresBoot && h.ctx.Err() == nil && h.expires.After(time.Now())
	h.mu.Unlock()
	if !valid {
		http.NotFound(w, req)
		return
	}
	if open == nil {
		w.WriteHeader(http.StatusServiceUnavailable)
		return
	}
	ctx, cancel := context.WithCancel(h.ctx)
	stop := context.AfterFunc(req.Context(), cancel)
	defer stop()
	defer cancel()
	reader, err := open(ctx)
	if err != nil {
		http.Error(w, "media unavailable", http.StatusServiceUnavailable)
		return
	}
	defer reader.Close()
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Type", "application/octet-stream")
	// ServeContent owns RFC Range/suffix/HEAD handling; the reader drives piece
	// demand at each seek. Only the verified storage reader supplies its bytes.
	http.ServeContent(w, req, "media", time.Time{}, &authorityReader{ReadSeekCloser: reader, h: h})
}

type authorityReader struct {
	io.ReadSeekCloser
	h *Handle
}

func (r *authorityReader) Read(b []byte) (int, error) {
	if !r.h.authorized() {
		return 0, errors.New("authority unavailable")
	}
	n, err := r.ReadSeekCloser.Read(b)
	if !r.h.authorized() {
		return 0, errors.New("authority unavailable")
	}
	return n, err
}
func (h *Handle) authorized() bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	boot, err := elapsedNow()
	return err == nil && boot < h.expiresBoot && h.ctx.Err() == nil && time.Now().Before(h.expires)
}

func (r *Runtime) Close() error {
	r.mu.Lock()
	if r.closed {
		r.mu.Unlock()
		<-r.shutdownDone
		r.mu.Lock()
		err := r.shutdownError
		r.mu.Unlock()
		return err
	}
	r.closed = true
	handles := make([]*Handle, 0, len(r.handles))
	for _, h := range r.handles {
		handles = append(handles, h)
	}
	r.mu.Unlock()
	// Authority revocation is independent of joining storage/handshakes. One
	// blocked input must not leave another handle readable during shutdown.
	for _, h := range handles {
		h.cancel()
	}
	r.server.Close()
	for _, h := range handles {
		h.Close()
	}
	// Watchers can retire a handle before shutdown snapshots the live map.
	// Retain and join those tasks too, rather than falsely reporting success.
	r.mu.Lock()
	retiring := make([]chan struct{}, 0, len(r.retiring))
	for _, done := range r.retiring {
		retiring = append(retiring, done)
	}
	r.mu.Unlock()
	for _, done := range retiring {
		<-done
	}
	r.saveRouting()
	r.client.Close()
	err := r.cache.Close()
	r.mu.Lock()
	r.shutdownError = err
	close(r.shutdownDone)
	r.mu.Unlock()
	return err
}

type publicPeers struct{ allowPrivate bool }

func (publicPeers) NumRanges() int { return 1 }
func (p publicPeers) Lookup(ip net.IP) (iplist.Range, bool) {
	return iplist.Range{Description: "non-public destination"}, !p.allowPrivate && !publicIP(ip)
}
func publicIP(ip net.IP) bool {
	a, ok := netip.AddrFromSlice(ip)
	if !ok {
		return false
	}
	a = a.Unmap()
	if !a.IsGlobalUnicast() || a.IsPrivate() || a.IsLoopback() || a.IsLinkLocalUnicast() {
		return false
	}
	if a.Is6() && !netip.MustParsePrefix("2000::/3").Contains(a) {
		return false
	}
	for _, prefix := range deniedNetworks {
		if prefix.Contains(a) {
			return false
		}
	}
	return true
}

var deniedNetworks = func() []netip.Prefix {
	var out []netip.Prefix
	for _, s := range []string{"0.0.0.0/8", "100.64.0.0/10", "192.0.0.0/24", "192.0.2.0/24", "192.88.99.0/24", "192.31.196.0/24", "192.52.193.0/24", "192.175.48.0/24", "198.18.0.0/15", "198.51.100.0/24", "203.0.113.0/24", "224.0.0.0/3", "2001::/23", "2001:db8::/32", "2002::/16", "3fff::/16", "2620:4f:8000::/48"} {
		out = append(out, netip.MustParsePrefix(s))
	}
	return out
}()

type publicPacket struct {
	net.PacketConn
	allowPrivate bool
}

func (p *publicPacket) WriteTo(b []byte, addr net.Addr) (int, error) {
	a, ok := addr.(*net.UDPAddr)
	if !ok || a.Port == 0 || (!p.allowPrivate && !publicIP(a.IP)) {
		return 0, errors.New("tracker destination denied")
	}
	return p.PacketConn.WriteTo(b, addr)
}
func publicDial(privateFixture bool) func(context.Context, string, string) (net.Conn, error) {
	return func(ctx context.Context, network, address string) (net.Conn, error) {
		host, port, err := net.SplitHostPort(address)
		if err != nil {
			return nil, errors.New("invalid destination")
		}
		n, e := strconv.Atoi(port)
		if e != nil || n < 1 || n > 65535 {
			return nil, errors.New("invalid destination")
		}
		ips, err := net.DefaultResolver.LookupIPAddr(ctx, host)
		if err != nil {
			return nil, errors.New("destination lookup failed")
		}
		for _, ip := range ips {
			if !privateFixture && !publicIP(ip.IP) {
				return nil, errors.New("non-public destination")
			}
		}
		for _, ip := range ips {
			conn, e := (&net.Dialer{Timeout: 10 * time.Second}).DialContext(ctx, network, net.JoinHostPort(ip.IP.String(), port))
			if e == nil {
				return conn, nil
			}
		}
		return nil, errors.New("destination unreachable")
	}
}
