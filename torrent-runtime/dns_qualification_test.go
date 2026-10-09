// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"encoding/json"
	"net"
	"os"
	"testing"
	"time"
)

func TestDNSQualification(t *testing.T) {
	if os.Getenv("TORRENT_DNS_PROBE") != "1" {
		t.Skip("explicit environment diagnosis")
	}
	var results []map[string]any
	for _, host := range []string{"tracker.opentrackr.org", "open.stealth.si", "router.bittorrent.com"} {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		start := time.Now()
		ips, err := net.DefaultResolver.LookupIPAddr(ctx, host)
		cancel()
		results = append(results, map[string]any{"host": host, "ok": err == nil, "addresses": len(ips), "elapsed_ms": float64(time.Since(start).Microseconds()) / 1000})
	}
	b, _ := json.Marshal(results)
	t.Log(string(b))
}
