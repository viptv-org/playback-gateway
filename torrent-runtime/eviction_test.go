// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

func TestEvictionInvalidatesCompletionBeforeDeletingBytes(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	writePiece(t, s, info, 0, true)
	writePiece(t, s, info, 1, true)
	writePiece(t, s, info, 2, true)
	called := false
	c.beforeEvict = func(key string) {
		called = true
		// Callback reenters storage exactly as Piece.UpdateCompletion does.
		if s.Piece(info.Piece(0)).Completion().Complete {
			t.Error("eviction callback still sees a completed victim")
		}
		if _, err := os.Stat(c.path(key)); err != nil {
			t.Error("piece bytes were removed before completion invalidation")
		}
		if c.Stats().UsedBytes != 3*4096 {
			t.Error("invalidation released the victim's charge before removal")
		}
	}
	writePiece(t, s, info, 3, true)
	if !called {
		t.Fatal("eviction never notified completion")
	}
	if s.Piece(info.Piece(0)).Completion().Complete {
		t.Fatal("victim remained complete")
	}
}

func TestLiveEngineCompletionIsInvalidatedBeforeUnlink(t *testing.T) {
	seed, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h := readyHandle(t, r, raw)
	seen := make(chan bool, 128)
	r.cache.mu.Lock()
	original := r.cache.beforeEvict
	r.cache.beforeEvict = func(key string) {
		original(key)
		index, _ := strconv.ParseUint(key[41:], 16, 32)
		_, exists := os.Stat(r.cache.path(key))
		state := h.t.Piece(int(index)).State()
		valid := exists == nil && !state.Complete && !state.Marking && !state.Hashing
		seen <- valid
	}
	r.cache.mu.Unlock()
	connectSeed(h, seed)
	for i := 0; i < 20; i++ {
		fetchRange(t, h, int64(i*4096), int64(i*4096+4095))
	}
	if len(seen) == 0 {
		t.Fatal("fixture did not evict a live torrent piece")
	}
	for len(seen) > 0 {
		if !<-seen {
			t.Fatal("live completion was not revoked before unlink")
		}
	}
}

func TestConcurrentFirstWritesRecheckEntryAfterEvictionYields(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	for i := 0; i < 3; i++ {
		writePiece(t, s, info, i, true)
	}
	entered, release := make(chan struct{}), make(chan struct{})
	unblock := sync.OnceFunc(func() { close(release) })
	defer unblock()
	// testStorage uses a zero storage hash, independent of info.Hash().
	c.beforeEvict = func(key string) {
		if key == pieceKey("0000000000000000000000000000000000000000", 0) {
			close(entered)
			<-release
		}
	}
	p := s.Piece(info.Piece(3))
	first := make(chan error, 1)
	go func() { _, err := p.WriteAt(bytes.Repeat([]byte{7}, 512), 0); first <- err }()
	select {
	case <-entered:
	case <-time.After(time.Second):
		t.Fatal("eviction did not yield")
	}
	_, secondErr := p.WriteAt(bytes.Repeat([]byte{8}, 512), 512)
	unblock()
	if err := <-first; err != nil {
		t.Fatal("first writer failed after another allocated its piece:", err)
	}
	if secondErr != nil {
		t.Fatal(secondErr)
	}
	if err := p.MarkComplete(); err != nil {
		t.Fatal(err)
	}
	b := make([]byte, 1024)
	if _, err = s.NewReader().ReadAt(b, 3*1024); err != nil || !bytes.Equal(b, append(bytes.Repeat([]byte{7}, 512), bytes.Repeat([]byte{8}, 512)...)) {
		t.Fatal("concurrent writes lost bytes")
	}
	if c.Stats().UsedBytes > c.Stats().LimitBytes {
		t.Fatal("concurrent reservation exceeded budget")
	}
}

func TestCacheCloseFencesAnEvictionCallbackStillInFlight(t *testing.T) {
	root := t.TempDir()
	c, err := OpenCache(root, 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	for i := 0; i < 3; i++ {
		writePiece(t, s, info, i, true)
	}
	entered, release := make(chan struct{}), make(chan struct{})
	unblock := sync.OnceFunc(func() { close(release) })
	defer unblock()
	c.beforeEvict = func(string) { close(entered); <-release }
	done := make(chan error, 1)
	go func() { _, err := s.Piece(info.Piece(3)).WriteAt([]byte{1}, 0); done <- err }()
	select {
	case <-entered:
	case <-time.After(time.Second):
		t.Fatal("eviction did not yield")
	}
	if n, err := s.NewReader().ReadAt(make([]byte, 1), 0); n != 0 || err != io.EOF {
		t.Fatal("evicting bytes remained readable")
	}
	if err := s.Close(); err != nil {
		t.Fatal(err)
	}
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	replacement, err := OpenCache(root, 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer replacement.Close()
	unblock()
	if err := <-done; !errors.Is(err, os.ErrClosed) {
		t.Fatal("retired writer was not fenced:", err)
	}
	if replacement.Stats().UsedBytes != 3*4096 {
		t.Fatal("old owner mutated new owner's cache")
	}
}

func TestUnlinkFailureTerminatesCacheWaiters(t *testing.T) {
	root := t.TempDir()
	c, err := OpenCache(root, 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	for i := 0; i < 3; i++ {
		writePiece(t, s, info, i, true)
	}
	failed := make(chan struct{}, 1)
	c.onFailure = func() { failed <- struct{}{} }
	c.beforeEvict = func(key string) {
		// A conflicting nonempty directory reliably refuses unlink on every
		// supported OS, including when a test runner has elevated permissions.
		path := c.path(key)
		if err := os.Rename(path, filepath.Join(root, "held")); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(path, "child"), []byte{1}, 0600); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := s.Piece(info.Piece(3)).WriteAt([]byte{1}, 0); !errors.Is(err, ErrCacheUnavailable) {
		t.Fatal("unlink failure did not terminalize cache:", err)
	}
	select {
	case <-failed:
	case <-time.After(time.Second):
		t.Fatal("cache failure was not reported")
	}
	done := make(chan error, 1)
	go func() { _, err := s.Piece(info.Piece(0)).WriteAt([]byte{1}, 0); done <- err }()
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("failed cache accepted a write")
		}
	case <-time.After(time.Second):
		t.Fatal("failed eviction left an immortal transient waiter")
	}
	if s.Piece(info.Piece(1)).Completion().Complete {
		t.Fatal("failed cache still advertised availability")
	}
}

func TestEvictionCompletionSharesFinalCloseLifetimeFence(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	first := readyHandle(t, r, raw)
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	info, err := mi.UnmarshalInfo()
	if err != nil {
		t.Fatal(err)
	}
	info.Name = "independent-input.bin"
	mi.InfoBytes, err = bencode.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	var buffer bytes.Buffer
	if err = mi.Write(&buffer); err != nil {
		t.Fatal(err)
	}
	second := readyHandle(t, r, buffer.Bytes())
	key := pieceKey(first.t.InfoHash().HexString(), 0)
	started, done := make(chan struct{}), make(chan struct{})
	r.mu.Lock()
	go func() { close(started); r.cache.beforeEvict(key); close(done) }()
	<-started
	escaped := false
	select {
	case <-done:
		escaped = true
	case <-time.After(20 * time.Millisecond):
	}
	r.mu.Unlock()
	if escaped {
		t.Fatal("completion synchronization escaped the Drop lifetime fence")
	}
	first.Close()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("eviction callback did not settle after Drop")
	}
	if !second.authorized() {
		t.Fatal("independent input lost authority")
	}
}
