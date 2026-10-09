// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

func TestPrivateControllerDoesNotExposeSourceOrAllowRenewAfterClose(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	c := &Controller{runtime: r, handles: map[string]*Handle{}}
	request, _ := json.Marshal(command{Version: 2, Op: "prepare", Source: &Source{Metainfo: raw}, AuthorityMillis: 60000})
	response := c.Call(request)
	var ready reply
	if json.Unmarshal(response, &ready) != nil || !ready.OK || ready.Handle == "" {
		t.Fatalf("prepare failed: %s", response)
	}
	if bytes.Contains(response, raw) || bytes.Contains(response, []byte("magnet")) {
		t.Fatal("source disclosed in response")
	}
	request, _ = json.Marshal(command{Version: 2, Op: "close", Handle: ready.Handle})
	var stopped reply
	json.Unmarshal(c.Call(request), &stopped)
	if !stopped.OK {
		t.Fatal("close refused")
	}
	request, _ = json.Marshal(command{Version: 2, Op: "renew", Handle: ready.Handle, AuthorityMillis: 60000})
	var renewed reply
	json.Unmarshal(c.Call(request), &renewed)
	if renewed.OK {
		t.Fatal("revoked authority renewed")
	}
	if response = c.Call([]byte(`{"version":2,"op":"observe","private_url":"secret-fixture"}`)); bytes.Contains(response, []byte("secret-fixture")) {
		t.Fatal("invalid control echoed private content")
	}
}

func TestWorkerSettlementDeadlineTerminatesStalledOwnedProcess(t *testing.T) {
	root := t.TempDir()
	cmd := exec.Command(os.Args[0], "-test.run=^TestSettlementChild$", "-test.timeout=10s")
	cmd.Env = append(os.Environ(), "TORRENT_SETTLEMENT_CHILD="+root)
	start := time.Now()
	err := cmd.Run()
	if err == nil {
		t.Fatal("stalled worker returned success")
	}
	if exit, ok := err.(*exec.ExitError); !ok || exit.ExitCode() != 3 {
		t.Fatalf("unexpected worker result: %v", err)
	}
	if elapsed := time.Since(start); elapsed > 4*time.Second {
		t.Fatal("worker termination exceeded deadline")
	}
	c, err := OpenCache(filepath.Join(root, "cache"), 32<<10)
	if err != nil {
		t.Fatal("dead worker cache could not be reclaimed:", err)
	}
	c.Close()
}

func TestSettlementChild(t *testing.T) {
	root := os.Getenv("TORRENT_SETTLEMENT_CHILD")
	if root == "" {
		t.Skip("owned child fixture only")
	}
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: filepath.Join(root, "cache"), CacheBytes: 32 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	h := readyHandle(t, r, raw)
	c := &Controller{runtime: r, handles: map[string]*Handle{"1": h}}
	r.cache.mu.Lock() // Models synchronous storage IO which cannot join.
	response := c.Call([]byte(`{"version":2,"op":"close","handle":"1"}`))
	var result reply
	json.Unmarshal(response, &result)
	if result.Terminate && !result.OK && result.Error == "settlement_timeout" {
		os.Exit(3)
	}
	os.Exit(2)
}

func TestRetiredControllerRecordsRemainBoundedUntilExplicitClose(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, MaxHandles: 1}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	c := &Controller{runtime: r, handles: map[string]*Handle{}}
	prepare, _ := json.Marshal(command{Version: 2, Op: "prepare", Source: &Source{Metainfo: raw}, AuthorityMillis: 60000})
	var first reply
	json.Unmarshal(c.Call(prepare), &first)
	if !first.OK {
		t.Fatal("initial handle refused")
	}
	c.handles[first.Handle].Close()
	var limited reply
	json.Unmarshal(c.Call(prepare), &limited)
	if limited.OK || limited.Error != "handle_limit" {
		t.Fatal("retired records escaped controller limit")
	}
	closeRequest, _ := json.Marshal(command{Version: 2, Op: "close", Handle: first.Handle})
	var closed reply
	json.Unmarshal(c.Call(closeRequest), &closed)
	if !closed.OK {
		t.Fatal("retired handle cleanup refused")
	}
	var next reply
	json.Unmarshal(c.Call(prepare), &next)
	if !next.OK {
		t.Fatal("explicit close did not release controller admission")
	}
}
