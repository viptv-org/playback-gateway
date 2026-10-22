// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"errors"
	"os"
	"sync"
	"testing"
	"time"

	"github.com/anacrolix/torrent/metainfo"
)

func waitForCacheIO(t *testing.T, c *Cache) {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		c.mu.Lock()
		n := c.activeIO
		c.mu.Unlock()
		if n > 0 {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("IO did not acquire its lifetime pin")
}

func TestBlockedPieceDoesNotBlockUnrelatedPlayback(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	want := writePiece(t, s, info, 0, true)
	writePiece(t, s, info, 1, false)
	e := c.entries[pieceKey("0000000000000000000000000000000000000000", 1)]
	e.ioMu.Lock()
	release := sync.OnceFunc(e.ioMu.Unlock)
	defer release()
	done := make(chan error, 1)
	go func() { done <- s.Piece(info.Piece(1)).MarkComplete() }()
	waitForCacheIO(t, c)
	read := make(chan error, 1)
	go func() {
		b := make([]byte, len(want))
		_, err := s.NewReader().ReadAt(b, 0)
		if err == nil && !bytes.Equal(b, want) {
			err = errors.New("playback bytes changed")
		}
		read <- err
	}()
	select {
	case err := <-read:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("unrelated playback blocked behind completion")
	}
	release()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestCacheCloseRetainsNamespaceUntilAdmittedIOSettles(t *testing.T) {
	root := t.TempDir()
	c, err := OpenCache(root, 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	s, info := testStorage(t, c)
	writePiece(t, s, info, 0, true)
	e := c.entries[pieceKey("0000000000000000000000000000000000000000", 0)]
	e.ioMu.Lock()
	release := sync.OnceFunc(e.ioMu.Unlock)
	defer release()
	read := make(chan error, 1)
	go func() { _, err := s.NewReader().ReadAt(make([]byte, 1), 0); read <- err }()
	waitForCacheIO(t, c)
	closed := make(chan error, 1)
	go func() { closed <- c.Close() }()
	select {
	case <-c.stopped:
	case <-time.After(time.Second):
		t.Fatal("close did not revoke admission")
	}
	if replacement, err := OpenCache(root, 64<<10); err == nil {
		replacement.Close()
		t.Fatal("namespace released while old IO remained admitted")
	}
	release()
	if err := <-read; !errors.Is(err, os.ErrClosed) {
		t.Fatal("queued IO was not fenced:", err)
	}
	if err := <-closed; err != nil {
		t.Fatal(err)
	}
	if c.openFiles != 0 {
		t.Fatal("close leaked cached descriptors")
	}
	replacement, err := OpenCache(root, 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	replacement.Close()
}

func TestCompletionNotifiesOnlyReadersOfThatPiece(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	a, unwatchA := c.watchPiece(pieceKey("0000000000000000000000000000000000000000", 0))
	b, unwatchB := c.watchPiece(pieceKey("0000000000000000000000000000000000000000", 1))
	writePiece(t, s, info, 0, true)
	select {
	case <-a:
	default:
		t.Fatal("completion notification lost")
	}
	select {
	case <-b:
		t.Fatal("unrelated reader awakened")
	default:
	}
	unwatchA()
	unwatchB()
	if len(c.waiters) != 0 {
		t.Fatal("reader subscriptions leaked")
	}
}

func TestDescriptorReuseStaysBoundedAcrossManyVerifiedPieces(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	info := &metainfo.Info{Name: "owned-many-pieces", Length: 128 * 1024, PieceLength: 1024, Pieces: make([]byte, 128*20)}
	s, err := c.OpenTorrent(context.Background(), info, metainfo.Hash{})
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 128; i++ {
		writePiece(t, s, info, i, true)
		if c.openFiles > maxCachedPieceFiles {
			t.Fatal("descriptor cache exceeded its bound")
		}
	}
	reader := s.NewReader()
	for i := 127; i >= 0; i-- {
		b := make([]byte, 1024)
		if _, err := reader.ReadAt(b, int64(i*1024)); err != nil || !bytes.Equal(b, bytes.Repeat([]byte{byte(i + 1)}, 1024)) {
			t.Fatal("descriptor replacement changed verified bytes", i, err)
		}
		if c.openFiles > maxCachedPieceFiles {
			t.Fatal("read-side descriptor bound exceeded")
		}
	}
}
