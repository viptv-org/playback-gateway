// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"github.com/anacrolix/torrent"
	"io"
	"net"
	"net/http"
	"testing"
	"time"
)

// Private profiling: no media addresses, payloads, or public protocol changes.
type qualificationReader struct {
	io.ReadSeekCloser
	position int64
	first    bool
	emit     func(map[string]any)
}

func (r *qualificationReader) Read(b []byte) (int, error) {
	start := time.Now()
	position := r.position
	n, err := r.ReadSeekCloser.Read(b)
	r.position += int64(n)
	if r.first || time.Since(start) >= 5*time.Millisecond {
		r.emit(map[string]any{"event": "media_read", "offset": position, "bytes": n, "wait_ms": float64(time.Since(start).Microseconds()) / 1000, "first": r.first})
	}
	r.first = false
	return n, err
}
func (r *qualificationReader) Seek(offset int64, whence int) (int64, error) {
	position, err := r.ReadSeekCloser.Seek(offset, whence)
	if err == nil {
		r.position = position
	}
	return position, err
}
func wrapQualificationReader(open func(context.Context) (io.ReadSeekCloser, error), emit func(map[string]any)) func(context.Context) (io.ReadSeekCloser, error) {
	return func(ctx context.Context) (io.ReadSeekCloser, error) {
		r, err := open(ctx)
		if err != nil {
			return nil, err
		}
		return &qualificationReader{ReadSeekCloser: r, first: true, emit: emit}, nil
	}
}

func TestConcurrentDiscoveryDiagnosticsAndPeerAdmission(t *testing.T) {
	raw, _, _ := wireMetainfo(t)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 256 << 10, clientTuning: func(c *torrent.ClientConfig) { c.DialForPeerConns = false }}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	done := make(chan struct{})
	go func() {
		defer close(done)
		for i := 0; i < 1000; i++ {
			h.t.AddPeers([]torrent.PeerInfo{{Addr: &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 30000 + i}}})
		}
	}()
	for i := 0; i < 1000; i++ {
		_ = h.Diagnostics()
	}
	<-done
}

// Emulate the real demand-retirement/write interleaving, without fabricating
// upstream chunk bits: the peer sends a valid late chunk, storage drops it, and
// the network layer still accounts it as received. Another peer gates unrelated
// chunks so an exact return range cannot hide behind a whole-piece rehash.
func TestSeekBackRedownloadsDiscardedPartialChunk(t *testing.T) {
	raw, data, hash := wireMetainfo(t)
	gateA := make(chan struct{}, 1)
	defer close(gateA)
	gateB := make(chan struct{})
	defer close(gateB)
	a := startWireFixture(t, data, hash, "127.0.0.1", gateA, false, false)
	b := startWireFixtureAt(t, data, hash, "127.0.0.1", gateB, false, false, 16<<10)
	r, h := wireRuntime(t, raw)
	h.t.AddPeers([]torrent.PeerInfo{{Addr: a.listener.Addr()}})
	req, _ := http.NewRequestWithContext(h.ctx, "GET", h.MediaURL(), nil)
	req.Header.Set("Range", "bytes=0-32767")
	response, err := (&http.Client{Timeout: 5 * time.Second}).Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	prefix := make([]byte, 16<<10)
	if _, err := io.ReadFull(response.Body, prefix); err != nil {
		t.Fatal(err)
	}
	// This is exactly the cache side of retiring an obsolete range, before the
	// in-flight network write finishes. No torrent state is altered by the test.
	r.cache.setDemand(hash.HexString(), map[int]bool{}, map[int]bool{})
	gateA <- struct{}{}
	deadline := time.Now().Add(time.Second)
	for h.t.BytesCompleted() < 32<<10 {
		if time.Now().After(deadline) {
			t.Fatal("late block was not accepted by the network layer")
		}
		time.Sleep(time.Millisecond)
	}
	response.Body.Close()
	for {
		r.mu.Lock()
		empty := len(r.demands) == 0
		r.mu.Unlock()
		if empty {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("old range did not retire")
		}
		time.Sleep(time.Millisecond)
	}
	done := make(chan wireReadResult, 1)
	go func() {
		request, _ := http.NewRequestWithContext(h.ctx, "GET", h.MediaURL(), nil)
		request.Header.Set("Range", "bytes=16384-17407")
		resp, err := (&http.Client{Timeout: 3 * time.Second}).Do(request)
		if err != nil {
			done <- wireReadResult{err: err}
			return
		}
		defer resp.Body.Close()
		body, err := io.ReadAll(resp.Body)
		done <- wireReadResult{body: body, err: err, status: resp.StatusCode}
	}()
	h.t.AddPeers([]torrent.PeerInfo{{Addr: b.listener.Addr()}})
	select {
	case first := <-b.requests:
		if first != 16<<10 {
			t.Fatalf("seek trusted a discarded chunk; requested unrelated offset %d", first)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("missing seek block was not requested")
	}
	select {
	case got := <-done:
		if h.repairedChunks.Load() == 0 {
			t.Fatal("fixture did not exercise storage repair")
		}
		if got.err != nil || got.status != 206 || !bytes.Equal(got.body, data[16384:17408]) {
			t.Fatalf("seek data unavailable: %v", got.err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("seek waited on unrelated gated blocks")
	}
}
