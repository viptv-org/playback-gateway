//go:build linux

// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"crypto/sha1"
	"encoding/json"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
	"golang.org/x/sys/unix"
)

// The runner owns a small mounted filesystem. This test uses the production
// free-space reserve; it never changes system mount or network policy itself.
func TestFileLargerThanCacheFilesystemStreamsAndReusesVerifiedPieces(t *testing.T) {
	root := os.Getenv("TORRENT_SMALL_FILESYSTEM")
	if root == "" {
		t.Skip("explicit owned small-filesystem fixture only")
	}
	if !filepath.IsAbs(root) {
		t.Fatal("owned filesystem must be absolute")
	}
	var geometry unix.Statfs_t
	if err := unix.Statfs(root, &geometry); err != nil {
		t.Fatal(err)
	}
	capacity := int64(geometry.Blocks) * int64(geometry.Bsize)
	if capacity <= 512<<20 || capacity > 768<<20 {
		t.Fatal("fixture requires a 512–768 MiB owned filesystem")
	}
	const length int64 = 1 << 30
	const pieceLength int64 = 256 << 10
	path := filepath.Join(t.TempDir(), "generated-zero-media.bin")
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	if err = file.Truncate(length); err != nil {
		file.Close()
		t.Fatal(err)
	}
	if err = file.Close(); err != nil {
		t.Fatal(err)
	}
	// The sparse source is an original zero pattern. Its real seeder verifies
	// every piece independently; a known digest keeps metadata generation cheap.
	digest := sha1.Sum(make([]byte, pieceLength))
	info := metainfo.Info{Name: filepath.Base(path), Length: length, PieceLength: pieceLength,
		Pieces: bytes.Repeat(digest[:], int(length/pieceLength))}
	encoded, err := bencode.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	mi := &metainfo.MetaInfo{InfoBytes: encoded}
	var metainfoBytes bytes.Buffer
	if err = mi.Write(&metainfoBytes); err != nil {
		t.Fatal(err)
	}
	config := torrent.NewDefaultClientConfig()
	config.DataDir = filepath.Dir(path)
	config.NoDHT, config.DisableTrackers, config.DisableIPv6, config.Seed = true, true, true, true
	config.NoDefaultPortForwarding = true
	config.ListenHost = func(string) string { return "127.0.0.1" }
	config.ListenPort = 0
	config.Slogger = slog.New(slog.NewTextHandler(io.Discard, nil))
	seed, err := torrent.NewClient(config)
	if err != nil {
		t.Fatal(err)
	}
	defer seed.Close()
	seeded, err := seed.AddTorrent(mi)
	if err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(30 * time.Second)
	for seeded.BytesCompleted() != length {
		if time.Now().After(deadline) {
			t.Fatal("real sparse seeder verification did not finish")
		}
		time.Sleep(10 * time.Millisecond)
	}
	cache := filepath.Join(root, "verified")
	r, err := newRuntime(Config{CacheDir: cache, CacheBytes: 2 << 30, ReadaheadBytes: 1 << 20}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if r != nil {
			r.Close()
		}
	}()
	h := readyHandle(t, r, metainfoBytes.Bytes())
	connectSeed(h, seed)
	if h.Observe().Length <= capacity {
		t.Fatal("selected file must exceed the entire cache filesystem")
	}
	limit := r.cache.Stats().LimitBytes
	if limit >= capacity || limit > capacity-(512<<20) {
		t.Fatal("native default did not preserve free-space reserve")
	}
	var peak int64
	for n := int64(0); n < 16; n++ {
		offset := n * (32 << 20)
		data := fetchRange(t, h, offset, offset+(8<<20)-1)
		if len(data) != 8<<20 || bytes.Count(data, []byte{0}) != len(data) {
			t.Fatal("verified range bytes differ")
		}
		stats := r.cache.Stats()
		peak = max(peak, stats.UsedBytes)
		if stats.UsedBytes > limit {
			t.Fatal("cache charge exceeded its reduced capacity")
		}
		if err := unix.Statfs(root, &geometry); err != nil {
			t.Fatal(err)
		}
		if int64(geometry.Bavail)*int64(geometry.Bsize) < 512<<20 {
			t.Fatal("filesystem free-space reserve was exceeded")
		}
	}
	transferred := h.t.Stats()
	if transferred.BytesReadData.Int64() >= length {
		t.Fatal("reader selection downloaded the entire file")
	}
	// Redownload an evicted beginning, retain it, then prove a fresh authorized
	// handle can read it after runtime restart without any seeder.
	if data := fetchRange(t, h, 0, 4095); !bytes.Equal(data, make([]byte, 4096)) {
		t.Fatal("evicted range did not recover")
	}
	data := fetchRange(t, h, length-4096, length-1)
	if !bytes.Equal(data, make([]byte, 4096)) {
		t.Fatal("EOF range did not recover")
	}
	r.Close()
	r = nil
	seed.Close()
	r, err = newRuntime(Config{CacheDir: cache, CacheBytes: 2 << 30, ReadaheadBytes: 1 << 20}, true)
	if err != nil {
		t.Fatal(err)
	}
	h = readyHandle(t, r, metainfoBytes.Bytes())
	if !bytes.Equal(fetchRange(t, h, length-4096, length-1), make([]byte, 4096)) {
		t.Fatal("restart lost verified EOF bytes")
	}
	metrics, _ := json.Marshal(map[string]int64{"file_bytes": length, "filesystem_bytes": capacity, "cache_limit_bytes": limit, "peak_cache_charge_bytes": peak})
	t.Log(string(metrics))
}
