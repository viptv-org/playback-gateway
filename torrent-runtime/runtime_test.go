// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

func seedFixture(t *testing.T, utpOnly bool) (*torrent.Client, []byte, []byte) {
	t.Helper()
	dir := t.TempDir()
	data := make([]byte, 256<<10)
	for i := range data {
		data[i] = byte(i*13 + i/4096)
	}
	path := filepath.Join(dir, "media.bin")
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	client, raw := seedPath(t, path, utpOnly)
	return client, raw, data
}

func seedPath(t *testing.T, path string, utpOnly bool) (*torrent.Client, []byte) {
	t.Helper()
	info := metainfo.Info{PieceLength: 4096}
	if err := info.BuildFromFilePath(path); err != nil {
		t.Fatal(err)
	}
	encoded, err := bencode.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	mi := &metainfo.MetaInfo{InfoBytes: encoded}
	var raw bytes.Buffer
	if err = mi.Write(&raw); err != nil {
		t.Fatal(err)
	}
	cfg := torrent.NewDefaultClientConfig()
	cfg.DataDir = filepath.Dir(path)
	cfg.NoDHT = true
	cfg.DisableTrackers = true
	cfg.NoDefaultPortForwarding = true
	cfg.ListenHost = func(string) string { return "127.0.0.1" }
	cfg.ListenPort = 0
	cfg.DisableIPv6 = true
	cfg.Seed = true
	cfg.DisableTCP = utpOnly
	cfg.Slogger = slog.New(slog.NewTextHandler(io.Discard, nil))
	client, err := torrent.NewClient(cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	seed, err := client.AddTorrent(mi)
	if err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for seed.BytesCompleted() != info.TotalLength() {
		if time.Now().After(deadline) {
			t.Fatal("seeder hash deadline")
		}
		time.Sleep(10 * time.Millisecond)
	}
	return client, raw.Bytes()
}

func readyHandle(t *testing.T, r *Runtime, raw []byte) *Handle {
	t.Helper()
	h, err := r.Prepare(context.Background(), Source{Metainfo: raw}, time.Now().Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}
	select {
	case <-h.done:
	case <-time.After(5 * time.Second):
		t.Fatal("prepare deadline")
	}
	if p := h.Observe(); !p.Ready {
		t.Fatalf("not ready: %+v", p)
	}
	return h
}
func connectSeed(h *Handle, seed *torrent.Client) {
	for _, addr := range seed.ListenAddrs() {
		h.t.AddPeers([]torrent.PeerInfo{{Addr: addr}})
	}
}

func fetchRange(t *testing.T, h *Handle, start, end int64) []byte {
	t.Helper()
	req, _ := http.NewRequest("GET", h.MediaURL(), nil)
	req.Header.Set("Range", fmt.Sprintf("bytes=%d-%d", start, end))

	resp, err := (&http.Client{Timeout: 10 * time.Second}).Do(req)
	if err != nil {
		t.Fatalf("range unavailable: discovery=%+v demand=%+v transfer=%+v", h.Diagnostics(), h.DemandDiagnostics(), h.t.Stats())
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != 206 {
		t.Fatalf("Range status %d", resp.StatusCode)
	}
	return b
}

func TestRealSwarmRollingRangeSeekAndIndependentAuthority(t *testing.T) {
	for _, utp := range []bool{false, true} {
		t.Run(fmt.Sprintf("utp_only_%v", utp), func(t *testing.T) {
			seed, raw, data := seedFixture(t, utp)
			r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 10, ReadaheadBytes: 4096}, true)
			if err != nil {
				t.Fatal(err)
			}
			defer r.Close()
			h := readyHandle(t, r, raw)
			other := readyHandle(t, r, raw)
			connectSeed(h, seed)
			for _, offset := range []int64{0, 220000, 32768, 180000, 0} {
				got := fetchRange(t, h, offset, offset+8191)
				if !bytes.Equal(got, data[offset:offset+8192]) {
					t.Fatal("selected range bytes differ")
				}
				if s := r.cache.Stats(); s.UsedBytes > s.LimitBytes {
					t.Fatal("cache exceeded hard bound")
				}
			}
			h.Close()
			resp, err := http.Get(h.MediaURL())
			if err != nil {
				t.Fatal(err)
			}
			resp.Body.Close()
			if resp.StatusCode != 404 {
				t.Fatal("closed capability remained live")
			}
			got := fetchRange(t, other, 0, 4095)
			if !bytes.Equal(got, data[:4096]) {
				t.Fatal("independent handle cancelled")
			}
			other.Close()
			r.mu.Lock()
			remaining := len(r.refs)
			r.mu.Unlock()
			if remaining != 0 {
				t.Fatal("torrent retained after final close")
			}
		})
	}
}

func TestRealVerifiedCacheReusedWithoutSeederAfterRestart(t *testing.T) {
	seed, raw, data := seedFixture(t, false)
	root := t.TempDir()
	r, err := newRuntime(Config{CacheDir: root, CacheBytes: 32 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	h := readyHandle(t, r, raw)
	connectSeed(h, seed)
	got := fetchRange(t, h, 0, 4095)
	if !bytes.Equal(got, data[:4096]) {
		t.Fatal("initial data")
	}
	r.Close()
	seed.Close()
	r, err = newRuntime(Config{CacheDir: root, CacheBytes: 32 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h = readyHandle(t, r, raw)
	got = fetchRange(t, h, 0, 4095)
	if !bytes.Equal(got, data[:4096]) {
		t.Fatal("cache reuse failed")
	}
}

func TestCloseCancelsBlockedReadAndExpiryCannotRevive(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	done := make(chan struct{})
	go func() {
		defer close(done)
		resp, e := http.Get(h.MediaURL())
		if e == nil {
			io.Copy(io.Discard, resp.Body)
			resp.Body.Close()
		}
	}()
	time.Sleep(50 * time.Millisecond)
	closed := make(chan struct{})
	go func() { h.Close(); close(closed) }()
	select {
	case <-closed:
	case <-time.After(2 * time.Second):
		t.Fatal("Close itself did not settle")
	}
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("blocked HTTP reader survived close")
	}
	if h.Renew(time.Now().Add(time.Minute)) == nil {
		t.Fatal("closed authority revived")
	}
}

func TestSourceIdentityAndSelectionValidation(t *testing.T) {
	for _, s := range []Source{{}, {Magnet: "secret-invalid-source"}, {Magnet: "magnet:?xt=urn:btih:0000000000000000000000000000000000000000"}} {
		if _, err := parseSource(s); err == nil {
			t.Fatal("invalid source admitted")
		}
	}
}

func TestBriefTCPPreferenceRetainsUTPOnlyPeerPlayback(t *testing.T) {
	seed, raw, data := seedFixture(t, true)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096, utpDialDelay: 50 * time.Millisecond}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	connectSeed(h, seed)
	if got := fetchRange(t, h, 0, 4095); !bytes.Equal(got, data[:4096]) {
		t.Fatal("uTP-only verified playback failed with a delayed dial")
	}
	for _, peer := range h.t.PeerConns() {
		if !strings.HasPrefix(peer.Network, "udp") && !strings.HasPrefix(peer.Network, "utp") {
			t.Fatal("uTP-only fixture connected over another transport")
		}
	}
}
