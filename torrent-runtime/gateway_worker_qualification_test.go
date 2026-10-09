// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bufio"
	"encoding/json"
	"fmt"
	"github.com/anacrolix/torrent"
	"golang.org/x/time/rate"
	"io"
	"net"
	"os"
	"testing"
	"time"
)

// Test-only stdio worker for owned local trackers/peers. It never enters platform artifacts.
func TestGatewayQualificationWorker(t *testing.T) {
	if os.Getenv("TORRENT_OWNED_WORKER") != "1" {
		t.Skip("owned process fixture only")
	}
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 65536), MaxControlBytes)
	controller := &Controller{}
	writer := bufio.NewWriter(os.Stdout)
	fmt.Fprintln(writer, `{"version":2,"ready":true,"qualification":"candidate"}`)
	writer.Flush()
	for scanner.Scan() {
		var request command
		data := scanner.Bytes()
		var response []byte
		if json.Unmarshal(data, &request) == nil && request.Version == 2 && request.Op == "open" && request.Config != nil {
			// Match the production streaming settings; only destination policy and DHT differ.
			request.Config.primeHeaders = true
			request.Config.clientTuning = func(c *torrent.ClientConfig) {
				c.DialRateLimiter = rate.NewLimiter(50, 50)
				c.HalfOpenConnsPerTorrent = 50
				c.NominalDialTimeout = 4 * time.Second
				c.MinDialTimeout = time.Second
			}
			runtime, err := newRuntime(*request.Config, true)
			if err != nil {
				response = []byte(`{"version":2,"ok":false,"error":"runtime_unavailable"}`)
			} else {
				controller.runtime = runtime
				controller.handles = map[string]*Handle{}
				response = []byte(`{"version":2,"ok":true}`)
			}
		} else if request.Op == "fixture_stats" && controller.runtime != nil {
			r := controller.runtime
			stats := map[string]any{"version": 2, "ok": true, "cache": r.cache.Stats()}
			peers, pieces, received := 0, 0, int64(0)
			for _, torrent := range r.client.Torrents() {
				s := torrent.Stats()
				peers += s.ActivePeers
				pieces += s.PiecesComplete
				received += s.BytesReadData.Int64()
			}
			stats["peers"] = peers
			stats["pieces"] = pieces
			stats["received"] = received
			awaitingFrame := 0
			for _, handle := range controller.handles {
				handle.mu.Lock()
				if !handle.firstFrame {
					awaitingFrame++
				}
				handle.mu.Unlock()
			}
			stats["awaiting_first_frame"] = awaitingFrame
			if handle := controller.handles[request.Handle]; handle != nil {
				handle.mu.Lock()
				stats["first_frame_reported"] = handle.firstFrame
				handle.mu.Unlock()
			}
			for _, t := range r.client.Torrents() {
				stats["known_peers"] = t.Stats().TotalPeers
				stats["half_open"] = t.Stats().HalfOpenPeers
				if info := t.Info(); info != nil {
					stats["tracker_count"] = len(t.Metainfo().AnnounceList)
					stats["piece_length"] = info.PieceLength
				}
			}
			response, _ = json.Marshal(stats)
		} else {
			response = controller.Call(data)
		}
		fmt.Fprintln(writer, string(response))
		writer.Flush()
		var reply struct {
			Terminate bool `json:"terminate"`
		}
		json.Unmarshal(response, &reply)
		if reply.Terminate || request.Op == "shutdown" {
			os.Exit(0)
		}
	}
	if controller.runtime != nil {
		controller.runtime.Close()
	}
	os.Exit(0)
}

// TestGatewayQualificationSeeder uses upstream's verified disk seeding implementation.
// The Rust fixture owns its pipes/process; ordinary test runs cannot launch it.
func TestGatewayQualificationSeeder(t *testing.T) {
	path := os.Getenv("TORRENT_OWNED_SEED_FILE")
	if path == "" {
		t.Skip("owned file seeder only")
	}
	client, raw := seedPath(t, path, false)
	port := 0
	for _, address := range client.ListenAddrs() {
		if tcp, ok := address.(*net.TCPAddr); ok {
			port = tcp.Port
			break
		}
	}
	if port == 0 {
		t.Fatal("fixture TCP listener unavailable")
	}
	reply, _ := json.Marshal(map[string]any{"peer_port": port, "metainfo": raw})
	fmt.Fprintln(os.Stdout, string(reply))
	io.Copy(io.Discard, os.Stdin)
	client.Close()
	os.Exit(0)
}
