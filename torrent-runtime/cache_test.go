// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/storage"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

func testStorage(t *testing.T, c *Cache) (storage.TorrentImpl, *metainfo.Info) {
	t.Helper()
	info := &metainfo.Info{Name: "fixture", Length: 16 * 1024, PieceLength: 1024, Pieces: make([]byte, 16*20)}
	s, err := c.OpenTorrent(context.Background(), info, metainfo.Hash{})
	if err != nil {
		t.Fatal(err)
	}
	return s, info
}
func writePiece(t *testing.T, s storage.TorrentImpl, info *metainfo.Info, index int, complete bool) []byte {
	t.Helper()
	data := bytes.Repeat([]byte{byte(index + 1)}, 1024)
	p := s.Piece(info.Piece(index))
	if _, err := p.WriteAt(data, 0); err != nil {
		t.Fatal(err)
	}
	if complete {
		if err := p.MarkComplete(); err != nil {
			t.Fatal(err)
		}
	}
	return data
}

func TestCacheEvictsAndRestoresVerifiedOnly(t *testing.T) {
	root := t.TempDir()
	limit := int64(3 * 4096)
	c, err := OpenCache(root, limit)
	if err != nil {
		t.Fatal(err)
	}
	s, info := testStorage(t, c)
	for i := 0; i < 12; i++ {
		writePiece(t, s, info, i, true)
		if c.Stats().UsedBytes > limit {
			t.Fatal("budget exceeded")
		}
	}
	if s.Piece(info.Piece(0)).Completion().Complete {
		t.Fatal("evicted piece still complete")
	}
	writePiece(t, s, info, 12, false)
	if err = c.Close(); err != nil {
		t.Fatal(err)
	}
	c, err = OpenCache(root, limit)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info = testStorage(t, c)
	if s.Piece(info.Piece(12)).Completion().Complete {
		t.Fatal("partial write restored")
	}
	if !s.Piece(info.Piece(11)).Completion().Complete {
		t.Fatal("verified piece lost")
	}
	b := make([]byte, 1024)
	if _, err = s.NewReader().ReadAt(b, 11*1024); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(b, bytes.Repeat([]byte{12}, 1024)) {
		t.Fatal("restored bytes differ")
	}
}

func TestCacheNeverServesReallocatedUnverifiedSlot(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	for i := 0; i < 4; i++ {
		writePiece(t, s, info, i, true)
	}
	writePiece(t, s, info, 0, false)
	n, err := s.NewReader().ReadAt(make([]byte, 1024), 0)
	if n != 0 || err != io.EOF {
		t.Fatalf("unverified data escaped: %d %v", n, err)
	}
}

func TestCachePinsAcrossHashReadsAndOwnsDirectory(t *testing.T) {
	root := t.TempDir()
	c, err := OpenCache(root, 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if other, err := OpenCache(root, 3*4096); err == nil {
		other.Close()
		t.Fatal("second owner admitted")
	}
	s, info := testStorage(t, c)
	writePiece(t, s, info, 0, true)
	r, err := s.Piece(info.Piece(0)).(storage.PieceReaderer).NewReader()
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	for i := 1; i < 12; i++ {
		writePiece(t, s, info, i, true)
	}
	if !s.Piece(info.Piece(0)).Completion().Complete {
		t.Fatal("pinned piece evicted")
	}
}

func TestCacheRejectsCorruptionOnRestart(t *testing.T) {
	root := t.TempDir()
	c, err := OpenCache(root, 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	s, info := testStorage(t, c)
	writePiece(t, s, info, 0, true)
	c.Close()
	files, _ := filepath.Glob(filepath.Join(root, "*.piece"))
	f, err := os.OpenFile(files[0], os.O_RDWR, 0600)
	if err != nil {
		t.Fatal(err)
	}
	f.WriteAt([]byte{99}, pieceHeaderBytes)
	f.Close()
	c, err = OpenCache(root, 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info = testStorage(t, c)
	if s.Piece(info.Piece(0)).Completion().Complete {
		t.Fatal("corrupt cache trusted")
	}
}

func TestCacheRecoversAfterProcessKilledWithIncompleteWrite(t *testing.T) {
	root := t.TempDir()
	cmd := exec.Command(os.Args[0], "-test.run=^TestCacheCrashChild$", "-test.timeout=10s")
	cmd.Env = append(os.Environ(), "TORRENT_CACHE_CRASH_CHILD="+root)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = cmd.Process.Kill(); _ = cmd.Wait() }()
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, err := os.Stat(filepath.Join(root, "ready")); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("crash fixture did not reach incomplete write")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if err := cmd.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	_ = cmd.Wait()
	c, err := OpenCache(filepath.Join(root, "cache"), 3*4096)
	if err != nil {
		t.Fatal("crashed owner retained cache lock:", err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	if !s.Piece(info.Piece(0)).Completion().Complete || s.Piece(info.Piece(1)).Completion().Complete {
		t.Fatal("restart failed to distinguish committed and incomplete pieces")
	}
	b := make([]byte, 1024)
	if _, err = s.NewReader().ReadAt(b, 0); err != nil || !bytes.Equal(b, bytes.Repeat([]byte{1}, 1024)) {
		t.Fatal("committed piece changed after process death")
	}
	if n, err := s.NewReader().ReadAt(b, 1024); n != 0 || err != io.EOF {
		t.Fatal("incomplete bytes escaped after process death")
	}
}

func TestCacheCrashChild(t *testing.T) {
	root := os.Getenv("TORRENT_CACHE_CRASH_CHILD")
	if root == "" {
		t.Skip("owned crash fixture only")
	}
	c, err := OpenCache(filepath.Join(root, "cache"), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	s, info := testStorage(t, c)
	writePiece(t, s, info, 0, true)
	if _, err = s.Piece(info.Piece(1)).WriteAt([]byte{7, 8, 9}, 0); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(root, "ready"), nil, 0600); err != nil {
		t.Fatal(err)
	}
	select {} // Parent kills this process; no Close or deferred cleanup runs.
}

func TestHashPinSurvivesReaderCloseUntilCompletionPublication(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 3*4096)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	writePiece(t, s, info, 0, false)
	p := s.Piece(info.Piece(0))
	r, err := p.(storage.PieceReaderer).NewReader()
	if err != nil {
		t.Fatal(err)
	}
	if _, err = io.Copy(io.Discard, io.NewSectionReader(r, 0, 1024)); err != nil {
		t.Fatal(err)
	}
	r.Close()
	// Upstream closes its hash reader before calling MarkComplete. Simulate
	// competing lookahead allocations precisely inside that publication gap.
	for i := 1; i < 8; i++ {
		writePiece(t, s, info, i, true)
	}
	if err = p.MarkComplete(); err != nil {
		t.Fatal("verified piece evicted before publication:", err)
	}
	if !p.Completion().Complete {
		t.Fatal("verified piece did not publish")
	}
	for i := 8; i < 12; i++ {
		writePiece(t, s, info, i, true)
	}
	if p.Completion().Complete {
		t.Fatal("published piece retained a verification pin forever")
	}
}
