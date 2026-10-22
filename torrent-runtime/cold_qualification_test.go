// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"runtime/debug"
	"sync"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
)

// Public-policy worker with test-only tuning/phase traces. No profiling command
// or tuning field is added to the production control protocol.
func TestColdQualificationWorker(t *testing.T) {
	profile := os.Getenv("TORRENT_COLD_PROFILE")
	if profile == "" {
		t.Skip("explicit cold-start experiment only")
	}
	debug.SetMemoryLimit(128 << 20)
	controller := &Controller{}
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 65536), MaxControlBytes)
	var monitors sync.WaitGroup
	var traceMu sync.Mutex
	var trace *os.File
	if path := os.Getenv("TORRENT_COLD_TRACE"); path != "" {
		var err error
		trace, err = os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
		if err != nil {
			t.Fatal("private trace unavailable")
		}
		defer trace.Close()
	}
	fmt.Println(`{"version":2,"ready":true,"qualification":"cold-experiment"}`)
	for scanner.Scan() {
		data := append([]byte(nil), scanner.Bytes()...)
		var request command
		_ = json.Unmarshal(data, &request)
		var response []byte
		if request.Op == "open" && request.Version == 2 && request.Config != nil {
			config := *request.Config
			switch profile {
			case "current":
			case "verified-only":
				config.verifiedPlaybackOnly = true
			case "head-only":
				config.primeHeadOnly = true
			case "head-plaintext":
				config.primeHeadOnly = true
				config.clientTuning = func(c *torrent.ClientConfig) { c.HeaderObfuscationPolicy.Preferred = false }
			case "plaintext-first":
				config.clientTuning = func(c *torrent.ClientConfig) { c.HeaderObfuscationPolicy.Preferred = false }
			case "tcp-head-start":
				config.utpDialDelay = 150 * time.Millisecond
			case "steal-grace":
				config.clientTuning = func(c *torrent.ClientConfig) { c.StealRequestGrace = 250 * time.Millisecond }
			case "no-prime":
				// Applied after runtime construction below; no source is admitted yet.
			default:
				t.Fatal("unknown cold-start profile")
			}
			r, err := New(config)
			if err != nil {
				response = []byte(`{"version":2,"ok":false,"error":"runtime_unavailable"}`)
			} else {
				if profile == "no-prime" {
					r.primeHeaders = false
				}
				controller.runtime = r
				controller.handles = map[string]*Handle{}
				response = []byte(`{"version":2,"ok":true}`)
			}
		} else {
			response = controller.Call(data)
		}
		if request.Op == "prepare" && trace != nil {
			var result reply
			if json.Unmarshal(response, &result) == nil && result.OK {
				h := controller.handles[result.Handle]
				monitors.Add(1)
				go func() {
					defer monitors.Done()
					ticker := time.NewTicker(50 * time.Millisecond)
					defer ticker.Stop()
					for {
						select {
						case <-h.ctx.Done():
							return
						case <-ticker.C:
							sample := map[string]any{"elapsed_ms": float64(time.Since(h.started).Microseconds()) / 1000,
								"discovery": h.Diagnostics(), "demand": h.DemandDiagnostics(), "progress": h.Observe()}
							traceMu.Lock()
							_ = json.NewEncoder(trace).Encode(sample)
							traceMu.Unlock()
						}
					}
				}()
			}
		}
		fmt.Println(string(response))
		var result reply
		_ = json.Unmarshal(response, &result)
		if result.Terminate {
			os.Exit(3)
		}
		if request.Op == "shutdown" {
			break
		}
	}
	if controller.runtime != nil {
		controller.runtime.Close()
	}
	monitors.Wait()
	os.Exit(0)
}
