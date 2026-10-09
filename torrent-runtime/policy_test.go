// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"github.com/anacrolix/torrent/metainfo"
	"net"
	"testing"
	"time"
)

func TestIncompleteStorageRetiresWithoutRestart(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	for i := 0; i < 3; i++ {
		p := s.Piece(info.Piece(i))
		if _, err = p.WriteAt([]byte{1}, 0); err != nil {
			t.Fatal(err)
		}
	}
	stale := s.Piece(info.Piece(0))
	if err = s.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err = stale.WriteAt([]byte{1}, 0); err == nil {
		t.Fatal("retired storage write accepted")
	}
	hash := metainfo.Hash{1}
	next, err := c.OpenTorrent(context.Background(), info, hash)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = next.Piece(info.Piece(0)).WriteAt([]byte{2}, 0); err != nil {
		t.Fatal("abandoned partials exhausted cache:", err)
	}
}

func TestPublicDestinationsAndUDPTrackerGuard(t *testing.T) {
	for _, address := range []string{"127.0.0.1", "10.0.0.1", "100.64.0.1", "169.254.169.254", "192.0.2.1", "198.18.1.1", "203.0.113.1", "240.1.1.1", "::1", "2001:db8::1", "2002:7f00:1::1", "3fff::1"} {
		if publicIP(net.ParseIP(address)) {
			t.Fatalf("reserved address admitted: %s", address)
		}
	}
	for _, address := range []string{"8.8.8.8", "1.1.1.1", "2001:4860:4860::8888"} {
		if !publicIP(net.ParseIP(address)) {
			t.Fatalf("public address refused: %s", address)
		}
	}
	pc, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer pc.Close()
	guard := &publicPacket{PacketConn: pc}
	if _, err = guard.WriteTo([]byte("fixture"), pc.LocalAddr()); err == nil {
		t.Fatal("UDP tracker escaped public policy")
	}
}

func TestTrackerDialDelegatesHostnameResolutionAndChecksActualDestination(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	_, port, err := net.SplitHostPort(listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	address := net.JoinHostPort("localhost", port)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if conn, err := publicDial(false)(ctx, "tcp4", address); err == nil {
		conn.Close()
		t.Fatal("resolved private tracker destination was connected")
	}
	if err = listener.(*net.TCPListener).SetDeadline(time.Now().Add(50 * time.Millisecond)); err != nil {
		t.Fatal(err)
	}
	if conn, err := listener.Accept(); err == nil {
		conn.Close()
		t.Fatal("destination guard ran after connecting")
	}
	if err = listener.(*net.TCPListener).SetDeadline(time.Time{}); err != nil {
		t.Fatal(err)
	}
	conn, err := publicDial(true)(ctx, "tcp4", address)
	if err != nil {
		t.Fatal("owned fixture hostname failed through the standard resolver")
	}
	defer conn.Close()
	accepted, err := listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	accepted.Close()
}

func TestStartupBudgetIncludesReadyEndpointUntilFirstFrame(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	for _, test := range []struct {
		code  string
		total bool
	}{{"startup_stalled", false}, {"startup_deadline", true}} {
		h := readyHandle(t, r, raw)
		h.mu.Lock()
		if test.total {
			h.started = time.Now().Add(-121 * time.Second)
		} else {
			h.progressAt = time.Now().Add(-61 * time.Second)
		}
		h.mu.Unlock()
		deadline := time.Now().Add(time.Second)
		for h.Observe().Error == "" && time.Now().Before(deadline) {
			time.Sleep(10 * time.Millisecond)
		}
		if h.Observe().Error != test.code {
			t.Fatalf("ready endpoint hid startup timeout: %+v", h.Observe())
		}
	}
}
