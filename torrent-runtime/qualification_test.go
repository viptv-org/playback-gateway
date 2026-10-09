// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// Explicit opt-in qualification server, compiled only into the test binary.
// Run the binary inside a fresh network namespace. It serves generated or
// operator-provided owned media, never configured production accounts/sources.
func TestQualificationFixture(t *testing.T) {
	dir := os.Getenv("TORRENT_QUALIFICATION_DIR")
	if dir == "" {
		t.Skip("explicit qualification runner only")
	}
	if !filepath.IsAbs(dir) {
		t.Fatal("qualification directory must be absolute")
	}
	if err := os.MkdirAll(dir, 0700); err != nil {
		t.Fatal(err)
	}
	var raw []byte
	var seederAddr net.Addr
	if media := os.Getenv("TORRENT_QUALIFICATION_MEDIA"); media != "" {
		seed, meta := seedPath(t, media, false)
		raw = meta
		for _, addr := range seed.ListenAddrs() {
			if addr.Network() == "tcp" {
				seederAddr = addr
				break
			}
		}
	} else {
		seed, meta, _ := seedFixture(t, false)
		raw = meta
		for _, addr := range seed.ListenAddrs() {
			if addr.Network() == "tcp" {
				seederAddr = addr
				break
			}
		}
	}
	if seederAddr == nil {
		t.Fatal("no owned TCP seeder")
	}
	l, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	origin := "http://" + l.Addr().String()
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	mi.Announce = origin + "/announce"
	var encoded bytes.Buffer
	mi.Write(&encoded)
	raw = encoded.Bytes()
	info, _ := mi.UnmarshalInfo()
	if err = os.WriteFile(filepath.Join(dir, "fixture.torrent"), raw, 0600); err != nil {
		t.Fatal(err)
	}
	var mu sync.Mutex
	var runtime *Runtime
	var handle *Handle
	var cachePath string
	create := func() {
		path, e := os.MkdirTemp(dir, "candidate-")
		if e != nil {
			t.Fatal(e)
		}
		cachePath = path
		runtime, e = newRuntime(Config{CacheDir: path, CacheBytes: 4 << 20, ReadaheadBytes: 256 << 10}, true)
		if e != nil {
			t.Fatal(e)
		}
	}
	create()
	defer func() { runtime.Close() }()
	write := func(w http.ResponseWriter, value any) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(value)
	}
	mux := http.NewServeMux()
	mux.HandleFunc("/fixture.torrent", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/x-bittorrent")
		w.Write(raw)
	})
	mux.HandleFunc("/announce", func(w http.ResponseWriter, r *http.Request) {
		addr := seederAddr.(*net.TCPAddr)
		peer := make([]byte, 6)
		copy(peer, addr.IP.To4())
		binary.BigEndian.PutUint16(peer[4:], uint16(addr.Port))
		value, _ := bencode.Marshal(map[string]any{"interval": int64(1), "complete": int64(1), "incomplete": int64(0), "peers": string(peer)})
		w.Write(value)
	})
	mux.HandleFunc("/candidate/start", func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		start := time.Now()
		var req struct {
			Cold      bool `json:"cold"`
			FileIndex int  `json:"file_index"`
			Magnet    bool `json:"magnet"`
		}
		if json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&req) != nil {
			w.WriteHeader(400)
			return
		}
		if handle != nil {
			handle.Close()
		}
		if req.Cold {
			runtime.Close()
			create()
		}
		source := Source{Metainfo: raw, FileIndex: &req.FileIndex}
		if req.Magnet {
			source.Metainfo = nil
			source.Magnet = "magnet:?xt=urn:btih:" + mi.HashInfoBytes().HexString()
			source.Trackers = []string{origin + "/announce"}
		}
		h, e := runtime.Prepare(context.Background(), source, time.Now().Add(time.Minute))
		if e != nil {
			write(w, map[string]any{"error": "candidate_admission_failed"})
			return
		}
		handle = h
		select {
		case <-h.done:
		case <-r.Context().Done():
			h.Close()
			return
		case <-time.After(30 * time.Second):
			h.Close()
			write(w, map[string]any{"error": "candidate_prepare_timeout"})
			return
		}
		p := h.Observe()
		write(w, map[string]any{"stream_url": h.MediaURL(), "file_size": p.Length, "file_index": p.FileIndex, "error": p.Error, "ready_elapsed_ms": float64(time.Since(start).Microseconds()) / 1000, "cache_directory": cachePath})
	})
	mux.HandleFunc("/candidate/stop", func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		start := time.Now()
		if handle != nil {
			handle.Close()
			handle = nil
		}
		write(w, map[string]any{"stop_ms": float64(time.Since(start).Microseconds()) / 1000, "cache": runtime.cache.Stats()})
	})
	server := &http.Server{Handler: mux, ReadHeaderTimeout: 5 * time.Second}
	go server.Serve(l)
	defer server.Close()
	config := map[string]any{"metainfo_url": origin + "/fixture.torrent", "tracker": origin + "/announce", "hash": mi.HashInfoBytes().HexString(), "file_index": 0, "length": info.TotalLength(), "seed_addr": seederAddr.String(), "candidate_start": origin + "/candidate/start", "candidate_stop": origin + "/candidate/stop", "metainfo_path": filepath.Join(dir, "fixture.torrent")}
	data, _ := json.Marshal(config)
	if err = os.WriteFile(filepath.Join(dir, "fixture.json"), data, 0600); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(25 * time.Minute)
	for time.Now().Before(deadline) {
		if _, err = os.Stat(filepath.Join(dir, "stop")); err == nil {
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
	t.Fatal("qualification fixture lifetime expired")
}
