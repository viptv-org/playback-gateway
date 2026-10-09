// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"net/http"
	"testing"
	"time"
)

func TestSuspendedAuthorityCannotReadRenewOrAcknowledgeFrame(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	// Wall-clock authority is still valid. Model suspension elapsed by the
	// boottime clock without waiting a minute or changing the system clock.
	h.mu.Lock()
	h.expiresBoot = 0
	h.mu.Unlock()
	if h.authorized() || h.Renew(time.Now().Add(time.Minute)) == nil || h.ReportFirstFrame() == nil {
		t.Fatal("suspended authority remained usable")
	}
	client := &http.Client{Timeout: time.Second}
	resp, err := client.Get(h.MediaURL())
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 200 && resp.StatusCode < 300 {
		t.Fatal("expired media endpoint accepted a reader")
	}
}
