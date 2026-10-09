// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"encoding/binary"
	"hash/crc32"
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
		{"rar5_compressed", "../engine/assets/rar-media/compressed.rar", nil, false, "compressed_archive_unsupported"},
		{"rar5_encrypted_headers", "../engine/assets/rar-selection/encrypted-headers.rar", nil, false, "encrypted_archive_unsupported"},
		{"rar5_encrypted_member", "../engine/assets/rar-selection/encrypted-member.rar", nil, false, "encrypted_archive_unsupported"},
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
			selection := h.Observe().ArchiveIndex
			if selection == nil || *selection != 0 {
				t.Fatal("resolved archive selection was not reported")
			}
			retained := h.restoreSelection(selectionRecord(h.t.InfoHash().HexString(), &index, nil))
			if retained == nil || retained.ArchiveIndex == nil || *retained.ArchiveIndex != 0 || retained.Length != int64(len(expected)) {
				t.Fatal("resolved archive member was not persisted")
			}
			*selection = 9
			if *h.Observe().ArchiveIndex != 0 {
				t.Fatal("observed selection mutated live identity")
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

func TestStoredMultipartArchiveMemberSelectionAndEveryVolumeRange(t *testing.T) {
	dir := t.TempDir()
	paths, err := filepath.Glob("../engine/assets/rar-selection/stored-multi.part*.rar")
	if err != nil || len(paths) != 7 {
		t.Fatal("seven-volume stored fixture unavailable")
	}
	for _, path := range paths {
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(filepath.Join(dir, filepath.Base(path)), data, 0600); err != nil {
			t.Fatal(err)
		}
	}
	seed, raw := seedPath(t, dir, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	for _, choice := range []int{-1, 0, 2, 7} {
		var member *int
		if choice >= 0 {
			index := choice
			member = &index
		}
		h, err := r.Prepare(context.Background(), Source{Metainfo: raw, ArchiveIndex: member}, time.Now().Add(time.Minute))
		if err != nil {
			t.Fatal(err)
		}
		connectSeed(h, seed)
		select {
		case <-h.done:
		case <-time.After(10 * time.Second):
			t.Fatal("archive selection deadline")
		}
		if choice == 7 {
			if got := h.Observe(); got.Ready || got.Error != "archive_index_unavailable" {
				t.Fatal("invalid member selection exposed media")
			}
			h.Close()
			continue
		}
		selected := choice
		if selected < 0 {
			selected = 1
		}
		got := h.Observe()
		if !got.Ready || got.ArchiveIndex == nil || *got.ArchiveIndex != selected {
			t.Fatalf("archive selection differs: %+v", got)
		}
		length := 24007
		if selected == 0 {
			length = 1289
		}
		expected := make([]byte, length)
		for n := range expected {
			if selected == 0 {
				expected[n] = byte(n % 131)
			} else if selected == 1 {
				expected[n] = byte(n % 251)
			} else {
				expected[n] = byte(250 - n%251)
			}
		}
		// A complete mapped read crosses all member volumes. Overlapping ranges
		// in both directions then cover every split with a seek across it.
		if !bytes.Equal(fetchRange(t, h, 0, int64(length-1)), expected) {
			t.Fatal("complete multipart member differs")
		}
		for _, reverse := range []bool{false, true} {
			for n := 0; n < length; n += 512 {
				start := n
				if reverse {
					start = max(0, length-1-n-1023)
				}
				end := min(length-1, start+1023)
				if !bytes.Equal(fetchRange(t, h, int64(start), int64(end)), expected[start:end+1]) {
					t.Fatal("multipart seek range differs")
				}
				if r.cache.Stats().UsedBytes > 32<<10 {
					t.Fatal("multipart selection exceeded piece budget")
				}
			}
		}
		h.Close()
	}
}

func TestEncryptedHeaderAndMalformedArchivesHaveSpecificRefusals(t *testing.T) {
	stored, err := os.ReadFile("../engine/assets/rar/stored.rar")
	if err != nil {
		t.Fatal(err)
	}
	// RAR4's 13-byte main header begins after its 7-byte signature. Set the
	// encrypted-header flag and recompute its real header CRC; this reaches
	// format parsing instead of merely constructing a FileHeader object.
	encrypted := append([]byte(nil), stored...)
	flags := binary.LittleEndian.Uint16(encrypted[10:12]) | 0x80
	binary.LittleEndian.PutUint16(encrypted[10:12], flags)
	binary.LittleEndian.PutUint16(encrypted[7:9], uint16(crc32.ChecksumIEEE(encrypted[9:20])))
	for _, fixture := range []struct {
		name, code string
		data       []byte
	}{
		{"encrypted-header", "encrypted_archive_unsupported", encrypted},
		{"malformed", "archive_metadata_invalid", []byte("invalid archive fixture")},
	} {
		t.Run(fixture.name, func(t *testing.T) {
			file := filepath.Join(t.TempDir(), "fixture.rar")
			if err := os.WriteFile(file, fixture.data, 0600); err != nil {
				t.Fatal(err)
			}
			seed, raw := seedPath(t, file, false)
			r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
			if err != nil {
				t.Fatal(err)
			}
			defer r.Close()
			h, err := r.Prepare(context.Background(), Source{Metainfo: raw}, time.Now().Add(time.Minute))
			if err != nil {
				t.Fatal(err)
			}
			connectSeed(h, seed)
			select {
			case <-h.done:
			case <-time.After(5 * time.Second):
				t.Fatal("invalid archive parser did not settle")
			}
			if h.Observe().Ready || h.Observe().Error != fixture.code {
				t.Fatalf("incorrect archive refusal: %+v", h.Observe())
			}
		})
	}
}
