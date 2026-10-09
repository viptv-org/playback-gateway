// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"errors"
	"io"
	"testing"
	"time"
)

func TestSeekReclaimsAbandonedPartialSlotsWithoutClosingTorrent(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	hash := "0000000000000000000000000000000000000000"
	c.setDemand(hash, map[int]bool{0: true, 1: true, 2: true}, map[int]bool{0: true})
	for i := 0; i < 3; i++ {
		if _, err = s.Piece(info.Piece(i)).WriteAt([]byte{1}, 0); err != nil {
			t.Fatal(err)
		}
	}
	c.setDemand(hash, map[int]bool{3: true}, map[int]bool{3: true})
	p := s.Piece(info.Piece(3))
	data := bytes.Repeat([]byte{7}, 1024)
	if _, err = p.WriteAt(data, 0); err != nil {
		t.Fatal("seek trapped by abandoned partials:", err)
	}
	if err = p.MarkComplete(); err != nil {
		t.Fatal(err)
	}
	before := c.Stats().UsedBytes
	if _, err = s.Piece(info.Piece(0)).WriteAt([]byte{2}, 512); err != nil {
		t.Fatal("late abandoned network write became a fatal storage error")
	}
	if c.Stats().UsedBytes != before {
		t.Fatal("late obsolete data reclaimed capacity")
	}
	if n, err := s.Piece(info.Piece(0)).ReadAt(make([]byte, 1024), 0); n != 0 || err != io.EOF {
		t.Fatal("dropped chunks became zero-filled data")
	}
	b := make([]byte, 1024)
	if _, err = s.NewReader().ReadAt(b, 3*1024); err != nil || !bytes.Equal(b, data) {
		t.Fatal("new seek data unavailable")
	}
}

func TestIndependentStalledHandleDoesNotBorrowAnotherReadersProgress(t *testing.T) {
	seed, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	a := readyHandle(t, r, raw)
	b := readyHandle(t, r, raw)
	connectSeed(a, seed)
	fetchRange(t, a, 0, 4095)
	b.mu.Lock()
	b.connected = true
	b.progressAt = time.Now().Add(-61 * time.Second)
	b.mu.Unlock()
	for i := 0; i < 50; i++ {
		fetchRange(t, a, int64(i*4096), int64(i*4096+4095))
		time.Sleep(20 * time.Millisecond)
	}
	if b.Observe().Error != "startup_stalled" {
		t.Fatal("another handle refreshed unrelated startup progress")
	}
	if !a.authorized() {
		t.Fatal("stalled handle retired independent reader")
	}
}

func TestRepeatedCachedRangeDoesNotRefreshStartupProgress(t *testing.T) {
	seed, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	connectSeed(h, seed)
	fetchRange(t, h, 0, 4095)
	before := time.Now().Add(-30 * time.Second)
	h.mu.Lock()
	h.connected = true
	h.progressAt = before
	h.mu.Unlock()
	fetchRange(t, h, 0, 4095)
	h.mu.Lock()
	afterReplay := h.progressAt
	h.mu.Unlock()
	if !afterReplay.Equal(before) {
		t.Fatal("replayed cache bytes extended the startup stall budget")
	}
	fetchRange(t, h, 4096, 8191)
	h.mu.Lock()
	afterNewBytes := h.progressAt
	h.mu.Unlock()
	if !afterNewBytes.After(before) {
		t.Fatal("new verified bytes did not count as progress")
	}
}

func TestHeadAdmissionCannotExceedSharedEngineWindow(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 512 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	info := h.t.Info()
	allowed := int(r.cache.payloadCapacity.Load() / info.PieceLength)
	if allowed+1 >= h.t.NumPieces() {
		t.Fatal("fixture cannot exercise the window boundary")
	}
	for i := 0; i <= allowed; i++ {
		reader := &demandReader{h: h, ctx: h.ctx, file: h.file}
		defer reader.Close()
		err := reader.updateDemand(pieceWindow{i, i + 1})
		if i < allowed && err != nil {
			t.Fatal("valid head rejected:", err)
		}
		if i == allowed && !errors.Is(err, ErrCapacity) {
			t.Fatal("head admitted beyond the engine request window")
		}
	}
}
