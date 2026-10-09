// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"crypto/sha1"
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

// An owned BEP 3 peer exercises the actual protocol and storage path without
// relying on an engine's seeder behaving correctly. No public swarm is used.
type wireFixture struct {
	listener      net.Listener
	ctx           context.Context
	cancel        context.CancelFunc
	joined        sync.WaitGroup
	data          []byte
	hash          metainfo.Hash
	requests      chan int
	gate          <-chan struct{}
	corrupt       bool
	dropHandshake bool
}

func wireMetainfo(t *testing.T) ([]byte, []byte, metainfo.Hash) {
	t.Helper()
	data := make([]byte, 64<<10)
	for i := range data {
		data[i] = byte(i*31 + i/123)
	}
	digest := sha1.Sum(data)
	info := metainfo.Info{Name: "owned-wire-fixture.bin", PieceLength: int64(len(data)), Length: int64(len(data)), Pieces: digest[:]}
	encoded, err := bencode.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	mi := &metainfo.MetaInfo{InfoBytes: encoded}
	var raw bytes.Buffer
	if err = mi.Write(&raw); err != nil {
		t.Fatal(err)
	}
	return raw.Bytes(), data, mi.HashInfoBytes()
}

func startWireFixture(t *testing.T, data []byte, hash metainfo.Hash, host string, gate <-chan struct{}, corrupt, drop bool) *wireFixture {
	t.Helper()
	listener, err := net.Listen("tcp4", host+":0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	f := &wireFixture{listener: listener, ctx: ctx, cancel: cancel, data: data, hash: hash, requests: make(chan int, 64), gate: gate, corrupt: corrupt, dropHandshake: drop}
	f.joined.Add(1)
	go func() {
		defer f.joined.Done()
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			f.joined.Add(1)
			go func() { defer f.joined.Done(); f.serve(conn) }()
		}
	}()
	t.Cleanup(func() { cancel(); listener.Close(); f.joined.Wait() })
	return f
}

func wireMessage(conn net.Conn, kind byte, payload []byte) error {
	message := make([]byte, 5+len(payload))
	binary.BigEndian.PutUint32(message[:4], uint32(1+len(payload)))
	message[4] = kind
	copy(message[5:], payload)
	_, err := io.Copy(conn, bytes.NewReader(message))
	return err
}

func (f *wireFixture) serve(conn net.Conn) {
	defer conn.Close()
	stop := context.AfterFunc(f.ctx, func() { conn.Close() })
	defer stop()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	var handshake [68]byte
	if _, err := io.ReadFull(conn, handshake[:]); err != nil {
		return
	}
	if f.dropHandshake {
		return
	}
	if handshake[0] != 19 || string(handshake[1:20]) != "BitTorrent protocol" || !bytes.Equal(handshake[28:48], f.hash[:]) {
		return
	}
	clear(handshake[20:28])
	copy(handshake[48:], []byte("-VTRQA1-owned-peer00"))
	if _, err := conn.Write(handshake[:]); err != nil {
		return
	}
	if wireMessage(conn, 5, []byte{0x80}) != nil || wireMessage(conn, 1, nil) != nil {
		return
	}
	for {
		var size [4]byte
		if _, err := io.ReadFull(conn, size[:]); err != nil {
			return
		}
		length := binary.BigEndian.Uint32(size[:])
		if length == 0 {
			continue
		}
		if length > 64<<10 {
			return
		}
		message := make([]byte, length)
		if _, err := io.ReadFull(conn, message); err != nil {
			return
		}
		if message[0] != 6 || len(message) != 13 {
			continue
		}
		index := binary.BigEndian.Uint32(message[1:5])
		begin := binary.BigEndian.Uint32(message[5:9])
		count := binary.BigEndian.Uint32(message[9:13])
		if index != 0 || count == 0 || count > 16<<10 || int64(begin)+int64(count) > int64(len(f.data)) {
			return
		}
		select {
		case f.requests <- int(begin):
		default:
		}
		if begin != 0 && f.gate != nil {
			select {
			case <-f.gate:
			case <-f.ctx.Done():
				return
			}
		}
		payload := make([]byte, 8+count)
		copy(payload[:8], message[1:9])
		copy(payload[8:], f.data[begin:begin+count])
		if f.corrupt && begin == 0 {
			payload[8] ^= 0xff
		}
		if wireMessage(conn, 7, payload) != nil {
			return
		}
	}
}

type wireReadResult struct {
	body   []byte
	err    error
	status int
}

func pendingWireRange(h *Handle) <-chan wireReadResult {
	done := make(chan wireReadResult, 1)
	go func() {
		req, _ := http.NewRequestWithContext(h.ctx, "GET", h.MediaURL(), nil)
		req.Header.Set("Range", "bytes=0-1023")
		resp, err := (&http.Client{Timeout: 8 * time.Second}).Do(req)
		if err != nil {
			done <- wireReadResult{err: err}
			return
		}
		defer resp.Body.Close()
		body, err := io.ReadAll(resp.Body)
		done <- wireReadResult{body: body, err: err, status: resp.StatusCode}
	}()
	return done
}

func wireRuntime(t *testing.T, raw []byte) (*Runtime, *Handle) {
	t.Helper()
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 256 << 10, ReadaheadBytes: 4096,
		clientTuning: func(c *torrent.ClientConfig) { c.HeaderObfuscationPolicy.Preferred = false; c.DisableUTP = true }}, true)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { r.Close() })
	return r, readyHandle(t, r, raw)
}

func requireWireRange(t *testing.T, result <-chan wireReadResult, data []byte) {
	t.Helper()
	select {
	case got := <-result:
		if got.err != nil || got.status != 206 || !bytes.Equal(got.body, data[:1024]) {
			t.Fatal("verified wire range was not delivered correctly")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("owned wire range did not recover")
	}
}

func TestSlowPeerCannotExposePrefixBeforeWholePieceVerification(t *testing.T) {
	raw, data, hash := wireMetainfo(t)
	release := make(chan struct{})
	peer := startWireFixture(t, data, hash, "127.0.0.1", release, false, false)
	_, h := wireRuntime(t, raw)
	h.t.AddPeers([]torrent.PeerInfo{{Addr: peer.listener.Addr()}})
	result := pendingWireRange(h)
	deadline := time.Now().Add(3 * time.Second)
	for h.DemandDiagnostics().WrittenHeadBytes == 0 && time.Now().Before(deadline) {
		time.Sleep(5 * time.Millisecond)
	}
	if h.DemandDiagnostics().WrittenHeadBytes == 0 {
		t.Fatal("slow peer never supplied its partial prefix")
	}
	select {
	case <-result:
		t.Fatal("partial piece reached media reader")
	case <-time.After(100 * time.Millisecond):
	}
	close(release)
	requireWireRange(t, result, data)
}

func TestCorruptWholePieceIsRefusedAndValidPeerRecovers(t *testing.T) {
	raw, data, hash := wireMetainfo(t)
	bad := startWireFixture(t, data, hash, "127.0.0.1", nil, true, false)
	_, h := wireRuntime(t, raw)
	h.t.AddPeers([]torrent.PeerInfo{{Addr: bad.listener.Addr()}})
	result := pendingWireRange(h)
	deadline := time.Now().Add(3 * time.Second)
	badCount := func() int64 { stats := h.t.Stats(); return stats.PiecesDirtiedBad.Int64() }
	for badCount() == 0 && time.Now().Before(deadline) {
		time.Sleep(5 * time.Millisecond)
	}
	if badCount() == 0 {
		t.Fatal("corrupt piece never reached verification")
	}
	select {
	case <-result:
		t.Fatal("corrupt piece reached media reader")
	case <-time.After(100 * time.Millisecond):
	}
	// Upstream bans the corrupt peer's IP; a distinct owned loopback address
	// proves recovery without clearing that integrity protection.
	good := startWireFixture(t, data, hash, "127.0.0.2", nil, false, false)
	h.t.AddPeers([]torrent.PeerInfo{{Addr: good.listener.Addr()}})
	requireWireRange(t, result, data)
}

func TestDroppedHandshakeDoesNotBlockHealthyPeerOrCancellation(t *testing.T) {
	raw, data, hash := wireMetainfo(t)
	dropped := startWireFixture(t, data, hash, "127.0.0.1", nil, false, true)
	_, h := wireRuntime(t, raw)
	h.t.AddPeers([]torrent.PeerInfo{{Addr: dropped.listener.Addr()}})
	good := startWireFixture(t, data, hash, "127.0.0.2", nil, false, false)
	h.t.AddPeers([]torrent.PeerInfo{{Addr: good.listener.Addr()}})
	requireWireRange(t, pendingWireRange(h), data)
	start := time.Now()
	h.Close()
	if time.Since(start) > 2*time.Second {
		t.Fatal("owned peer cancellation did not settle")
	}
}

func TestWorkerTerminationDisconnectsStalledHandshake(t *testing.T) {
	for _, mode := range []string{"close", "cancel-shutdown"} {
		t.Run(mode, func(t *testing.T) { stalledHandshakeWorker(t, mode) })
	}
}

func stalledHandshakeWorker(t *testing.T, mode string) {
	_, _, hash := wireMetainfo(t)
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	var mu sync.Mutex
	var connections []net.Conn
	var joined sync.WaitGroup
	accepted, disconnected := make(chan struct{}), make(chan struct{})
	var acceptOnce, disconnectOnce sync.Once
	joined.Add(1)
	go func() {
		defer joined.Done()
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			mu.Lock()
			connections = append(connections, conn)
			mu.Unlock()
			joined.Add(1)
			go func() {
				defer joined.Done()
				defer conn.Close()
				var handshake [68]byte
				if _, err := io.ReadFull(conn, handshake[:]); err != nil {
					return
				}
				acceptOnce.Do(func() { close(accepted) })
				// TCP works; the peer never answers its BitTorrent handshake.
				_, _ = io.Copy(io.Discard, conn)
				disconnectOnce.Do(func() { close(disconnected) })
			}()
		}
	}()
	t.Cleanup(func() {
		listener.Close()
		mu.Lock()
		for _, conn := range connections {
			conn.Close()
		}
		mu.Unlock()
		joined.Wait()
	})
	root := t.TempDir()
	config, _ := json.Marshal(map[string]string{"magnet": "magnet:?xt=urn:btih:" + hash.HexString(), "peer": listener.Addr().String(), "mode": mode})
	if err := os.WriteFile(filepath.Join(root, "input.json"), config, 0600); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(os.Args[0], "-test.run=^TestHandshakeSettlementChild$", "-test.timeout=10s")
	cmd.Env = append(os.Environ(), "TORRENT_HANDSHAKE_CHILD="+root)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = cmd.Process.Kill(); _ = cmd.Wait() }()
	select {
	case <-accepted:
	case <-time.After(3 * time.Second):
		t.Fatal("fixture did not reach stalled handshake")
	}
	if err := os.WriteFile(filepath.Join(root, "accepted"), nil, 0600); err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	err = cmd.Wait()
	exit, ok := err.(*exec.ExitError)
	if !ok || exit.ExitCode() != 3 {
		t.Fatal("stalled handshake worker did not terminate with its owned deadline")
	}
	if time.Since(start) > 4*time.Second {
		t.Fatal("stalled handshake termination exceeded allowance")
	}
	var result reply
	response, err := os.ReadFile(filepath.Join(root, "result.json"))
	if err != nil {
		t.Fatal(err)
	}
	if json.Unmarshal(response, &result) != nil || !result.Terminate || result.Error != "settlement_timeout" {
		t.Fatal("early successful cleanup hid a live handshake")
	}
	select {
	case <-disconnected:
	case <-time.After(500 * time.Millisecond):
		t.Fatal("terminated worker left a peer connection alive")
	}
	cache, err := OpenCache(filepath.Join(root, "cache"), 256<<10)
	if err != nil {
		t.Fatal("terminated worker retained its cache ownership")
	}
	cache.Close()
}

func TestHandshakeSettlementChild(t *testing.T) {
	root := os.Getenv("TORRENT_HANDSHAKE_CHILD")
	if root == "" {
		t.Skip("owned handshake worker only")
	}
	data, err := os.ReadFile(filepath.Join(root, "input.json"))
	if err != nil {
		t.Fatal("fixture input unavailable")
	}
	var config struct {
		Magnet string
		Peer   string
		Mode   string
	}
	if json.Unmarshal(data, &config) != nil {
		t.Fatal("fixture input invalid")
	}
	address, err := net.ResolveTCPAddr("tcp4", config.Peer)
	if err != nil {
		t.Fatal("fixture peer invalid")
	}
	r, err := newRuntime(Config{CacheDir: filepath.Join(root, "cache"), CacheBytes: 256 << 10,
		clientTuning: func(c *torrent.ClientConfig) { c.HeaderObfuscationPolicy.Preferred = false; c.DisableUTP = true }}, true)
	if err != nil {
		t.Fatal(err)
	}
	h, err := r.Prepare(context.Background(), Source{Magnet: config.Magnet}, time.Now().Add(time.Minute))
	if err != nil {
		t.Fatal("fixture preparation failed")
	}
	h.t.AddPeers([]torrent.PeerInfo{{Addr: address}})
	deadline := time.Now().Add(4 * time.Second)
	for {
		if _, err := os.Stat(filepath.Join(root, "accepted")); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("fixture handshake not observed")
		}
		time.Sleep(time.Millisecond)
	}
	c := &Controller{runtime: r, handles: map[string]*Handle{"1": h}}
	command := []byte(`{"version":2,"op":"close","handle":"1"}`)
	if config.Mode == "cancel-shutdown" {
		h.cancel()
		deadline := time.Now().Add(time.Second)
		for {
			r.mu.Lock()
			retired := r.handles[h.token] == nil
			r.mu.Unlock()
			if retired {
				break
			}
			if time.Now().After(deadline) {
				t.Fatal("cancelled handle did not revoke access")
			}
			time.Sleep(time.Millisecond)
		}
		command = []byte(`{"version":2,"op":"shutdown"}`)
	}
	response := c.Call(command)
	_ = os.WriteFile(filepath.Join(root, "result.json"), response, 0600)
	var result reply
	json.Unmarshal(response, &result)
	if result.Terminate {
		os.Exit(3)
	}
	os.Exit(2)
}
