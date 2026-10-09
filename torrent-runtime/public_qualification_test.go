// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"github.com/anacrolix/torrent"
	"golang.org/x/time/rate"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

// Opt-in real-swarm probe. Inputs, source identities, media digests and raw
// decoder output are private artifacts, never test output or tracked fixtures.
func TestPublicSwarmQualification(t *testing.T) {
	input := os.Getenv("TORRENT_REAL_SOURCES")
	if input == "" {
		t.Skip("explicit real-swarm qualification only")
	}
	output := os.Getenv("TORRENT_REAL_OUTPUT")
	if !filepath.IsAbs(output) {
		t.Fatal("private absolute output directory required")
	}
	if err := os.MkdirAll(output, 0700); err != nil {
		t.Fatal("private output unavailable")
	}
	data, err := os.ReadFile(input)
	if err != nil || len(data) > 1<<20 {
		t.Fatal("private input unavailable")
	}
	var corpus struct {
		Sources []struct {
			URL   string `json:"url"`
			Index *int   `json:"fileIdx"`
		}
	}
	if json.Unmarshal(data, &corpus) != nil {
		t.Fatal("invalid private corpus")
	}
	ordinal, _ := strconv.Atoi(os.Getenv("TORRENT_REAL_CASE"))
	if ordinal < 0 || ordinal >= len(corpus.Sources) {
		t.Fatal("invalid corpus ordinal")
	}
	result := map[string]any{"engine": "candidate-anacrolix-d913b30f520e", "case_ordinal": ordinal, "passed": false}
	defer func() {
		b, _ := json.MarshalIndent(result, "", "  ")
		_ = os.WriteFile(filepath.Join(output, "result.json"), b, 0600)
	}()
	cache := os.Getenv("TORRENT_REAL_CACHE")
	if cache == "" {
		cache = filepath.Join(output, "cache")
	}
	config := Config{CacheDir: cache, ReadaheadBytes: 4 << 20}
	profile := os.Getenv("TORRENT_REAL_PROFILE")
	result["connection_profile"] = profile
	switch profile {
	case "", "stock":
	case "plaintext":
		config.clientTuning = func(c *torrent.ClientConfig) { c.HeaderObfuscationPolicy.Preferred = false }
	case "dial50", "dial50-prime":
		config.primeHeaders = profile == "dial50-prime"
		config.clientTuning = func(c *torrent.ClientConfig) {
			c.DialRateLimiter = rate.NewLimiter(50, 50)
			c.HalfOpenConnsPerTorrent = 50
		}
	case "dial50-fast-prime", "fast-ipv4-prime", "fast-plaintext-prime", "fast-tcp-prime", "fast-tcp-preferred-prime":
		config.primeHeaders = true
		if profile == "fast-tcp-preferred-prime" {
			config.utpDialDelay = 150 * time.Millisecond
		}
		config.clientTuning = func(c *torrent.ClientConfig) {
			c.DialRateLimiter = rate.NewLimiter(50, 50)
			c.HalfOpenConnsPerTorrent = 50
			c.NominalDialTimeout = 4 * time.Second
			c.MinDialTimeout = time.Second
			c.DisableIPv6 = profile == "fast-ipv4-prime"
			if profile == "fast-plaintext-prime" {
				c.HeaderObfuscationPolicy.Preferred = false
			}
			if profile == "fast-tcp-prime" {
				c.DisableUTP = true
			}
		}
	case "plaintext-dial50":
		config.clientTuning = func(c *torrent.ClientConfig) {
			c.HeaderObfuscationPolicy.Preferred = false
			c.DialRateLimiter = rate.NewLimiter(50, 50)
			c.HalfOpenConnsPerTorrent = 50
		}
	default:
		t.Fatal("invalid qualification profile")
	}
	r, err := New(config)
	if err != nil {
		result["error"] = "runtime_unavailable"
		t.Fatal("runtime unavailable")
	}
	defer r.Close()
	start := time.Now()
	sustained := os.Getenv("TORRENT_REAL_SUSTAINED") == "1"
	probeBudget := 125 * time.Second
	if sustained {
		probeBudget = 205 * time.Second
	}
	ctx, cancel := context.WithTimeout(context.Background(), probeBudget)
	defer cancel()
	h, err := r.Prepare(ctx, Source{Magnet: corpus.Sources[ordinal].URL, FileIndex: corpus.Sources[ordinal].Index}, time.Now().Add(time.Minute))
	if err != nil {
		result["error"] = "admission_failed"
		t.Fatal("source admission failed")
	}
	defer h.Close()
	traceFile, traceError := os.OpenFile(filepath.Join(output, "numeric-trace.jsonl"), os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0600)
	if traceError != nil {
		t.Fatal("numeric trace unavailable")
	}
	traceCtx, traceCancel := context.WithCancel(ctx)
	traceDone := make(chan struct{})
	go func() {
		defer close(traceDone)
		defer traceFile.Close()
		ticker := time.NewTicker(250 * time.Millisecond)
		defer ticker.Stop()
		encoder := json.NewEncoder(traceFile)
		for {
			select {
			case <-traceCtx.Done():
				return
			case <-ticker.C:
				_ = encoder.Encode(map[string]any{"elapsed_ms": float64(time.Since(start).Microseconds()) / 1000, "discovery": h.Diagnostics(), "demand": h.DemandDiagnostics()})
			}
		}
	}()
	defer func() { traceCancel(); <-traceDone }()
	go func() {
		ticker := time.NewTicker(20 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-h.ctx.Done():
				return
			case <-ticker.C:
				_ = h.Renew(time.Now().Add(time.Minute))
			}
		}
	}()
	select {
	case <-h.done:
	case <-ctx.Done():
		result["error"] = "preparation_timeout"
		t.Fatal("preparation timeout")
	}
	result["metadata_ready_ms"] = float64(time.Since(start).Microseconds()) / 1000
	result["metadata_diagnostics"] = h.Diagnostics()
	status := h.Observe()
	stats := h.t.Stats()
	result["connected_peers"] = stats.ActivePeers
	transports := map[string]int{"tcp": 0, "utp": 0, "other": 0}
	for _, peer := range h.t.PeerConns() {
		switch {
		case strings.HasPrefix(peer.Network, "utp") || strings.HasPrefix(peer.Network, "udp"):
			transports["utp"]++
		case strings.HasPrefix(peer.Network, "tcp"):
			transports["tcp"]++
		default:
			transports["other"]++
		}
	}
	result["connected_transports"] = transports
	result["stage"] = status.Stage
	if !status.Ready {
		result["error"] = status.Error
		t.Fatal("real source preparation failed; details in private numeric result")
	}
	result["selected_size"] = status.Length
	result["selected_index"] = status.FileIndex
	client := &http.Client{Timeout: 45 * time.Second}
	var ranges []map[string]any
	offsets := []int64{0, status.Length / 2, 0}
	if os.Getenv("TORRENT_REAL_STARTUP_ONLY") == "1" {
		offsets = nil
		result["startup_only"] = true
	}
	for _, offset := range offsets {
		length := min(int64(256<<10), status.Length-offset)
		req, _ := http.NewRequestWithContext(ctx, "GET", h.MediaURL(), nil)
		req.Header.Set("Range", fmt.Sprintf("bytes=%d-%d", offset, offset+length-1))
		readStart := time.Now()
		resp, e := client.Do(req)
		if e != nil {
			result["error"] = "range_unavailable"
			result["progress"] = h.Observe()
			t.Fatal("real range unavailable")
		}
		body, e := io.ReadAll(io.LimitReader(resp.Body, length+1))
		resp.Body.Close()
		if e != nil || resp.StatusCode != 206 || int64(len(body)) != length {
			result["error"] = "range_incomplete"
			t.Fatal("real range incomplete")
		}
		digest := sha256.Sum256(body)
		ranges = append(ranges, map[string]any{"offset": offset, "bytes": len(body), "elapsed_ms": float64(time.Since(readStart).Microseconds()) / 1000, "sha256": hex.EncodeToString(digest[:])})
	}
	result["ranges"] = ranges
	decodeStart := time.Now()
	if sustained {
		measurement, err := sustainedDecode(ctx, h.MediaURL(), filepath.Join(output, "decoder-private.log"), start, h.ReportFirstFrame)
		result["sustained_decoder"] = measurement
		if err != nil {
			result["error"] = "sustained_decode_failed"
			t.Fatal("sustained decoding incomplete; inspect private evidence")
		}
	} else {
		cmd := exec.CommandContext(ctx, "ffmpeg", "-hide_banner", "-loglevel", "error", "-i", h.MediaURL(), "-frames:v", "1", "-an", "-f", "null", "-")
		log, e := cmd.CombinedOutput()
		_ = os.WriteFile(filepath.Join(output, "decoder-private.log"), log, 0600)
		result["decoder_ms"] = float64(time.Since(decodeStart).Microseconds()) / 1000
		result["first_frame_total_ms"] = float64(time.Since(start).Microseconds()) / 1000
		if e != nil {
			result["error"] = "first_frame_failed"
			result["progress"] = h.Observe()
			t.Fatal("real first frame unavailable")
		}
		if err = h.ReportFirstFrame(); err != nil {
			result["error"] = "first_frame_deadline"
			t.Fatal("real first frame missed startup authority")
		}
	}
	result["cache"] = r.cache.Stats()
	result["playback_diagnostics"] = h.Diagnostics()
	closeStart := time.Now()
	closed := make(chan struct{})
	go func() { h.Close(); close(closed) }()
	select {
	case <-closed:
	case <-time.After(2 * time.Second):
		result["error"] = "settlement_timeout"
		t.Fatal("real source did not settle")
	}
	result["stop_ms"] = float64(time.Since(closeStart).Microseconds()) / 1000
	result["passed"] = true
}
