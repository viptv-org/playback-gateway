// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestStoredArchiveTorrentRanges(t *testing.T) {
	for _, fixture := range []struct {
		name, pattern string
		expected      []byte
		missing       bool
		errorCode     string
	}{
		{"rar4", "../engine/assets/rar/stored.rar", bytes.Repeat([]byte("0123456789"), 10), false, ""},
		{"rar4_multipart", "../engine/assets/rar/split.part*.rar", bytes.Repeat([]byte("0123456789"), 1000), false, ""},
		{"compressed", "../engine/assets/rar/compressed.rar", nil, false, "compressed_archive_unsupported"},
		{"missing_volume", "../engine/assets/rar/split.part*.rar", nil, true, "archive_volume_missing"},
		{"rar5_multipart", "../engine/assets/rar-media/stored/clip.part*.rar", nil, false, ""},
	} {
		t.Run(fixture.name, func(t *testing.T) {
			dir := filepath.Join(t.TempDir(), "archive")
			os.Mkdir(dir, 0700)
			paths, err := filepath.Glob(fixture.pattern)
			if err != nil || len(paths) == 0 {
				t.Fatal("archive fixtures unavailable")
			}
			for i, p := range paths {
				if fixture.missing && i == 1 {
					continue
				}
				data, e := os.ReadFile(p)
				if e != nil {
					t.Fatal(e)
				}
				if e = os.WriteFile(filepath.Join(dir, filepath.Base(p)), data, 0600); e != nil {
					t.Fatal(e)
				}
			}
			seed, raw := seedPath(t, dir, false)
			r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 10, ReadaheadBytes: 4096}, true)
			if err != nil {
				t.Fatal(err)
			}
			defer r.Close()
			index := 0
			h, err := r.Prepare(context.Background(), Source{Metainfo: raw, FileIndex: &index}, time.Now().Add(time.Minute))
			if err != nil {
				t.Fatal(err)
			}
			connectSeed(h, seed)
			select {
			case <-h.done:
			case <-time.After(10 * time.Second):
				t.Fatal("archive preparation deadline")
			}
			if fixture.errorCode != "" {
				if h.Observe().Error != fixture.errorCode {
					t.Fatalf("expected %s, got %+v", fixture.errorCode, h.Observe())
				}
				return
			}
			if !h.Observe().Ready {
				t.Fatalf("archive unavailable: %+v", h.Observe())
			}
			expected := fixture.expected
			if fixture.name == "rar5_multipart" {
				expected, err = os.ReadFile("../engine/assets/rar-media/clip.mp4")
				if err != nil {
					t.Fatal(err)
				}
			}
			if h.Observe().Length != int64(len(expected)) {
				t.Fatal("member length mismatch")
			}
			for _, offset := range []int{0, len(expected) / 2, max(0, len(expected)-128), 0} {
				end := min(len(expected)-1, offset+127)
				got := fetchRange(t, h, int64(offset), int64(end))
				if !bytes.Equal(got, expected[offset:end+1]) {
					t.Fatal("archive mapped bytes differ")
				}
			}
			if r.cache.Stats().UsedBytes > 32<<10 {
				t.Fatal("archive exceeded cache budget")
			}
		})
	}
}
