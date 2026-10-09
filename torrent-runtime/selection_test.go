// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestExactSelectionSharedBoundarySuffixAndLargestTie(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "files")
	os.Mkdir(dir, 0700)
	contents := [][]byte{bytes.Repeat([]byte("a"), 5001), bytes.Repeat([]byte("b"), 9003), bytes.Repeat([]byte("c"), 9003)}
	for i, name := range []string{"a.bin", "b.bin", "c.bin"} {
		if err := os.WriteFile(filepath.Join(dir, name), contents[i], 0600); err != nil {
			t.Fatal(err)
		}
	}
	seed, raw := seedPath(t, dir, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	for _, choice := range []int{-1, 0, 2} {
		var index *int
		if choice >= 0 {
			v := choice
			index = &v
		}
		h, err := r.Prepare(context.Background(), Source{Metainfo: raw, FileIndex: index}, time.Now().Add(time.Minute))
		if err != nil {
			t.Fatal(err)
		}
		connectSeed(h, seed)
		select {
		case <-h.done:
		case <-time.After(3 * time.Second):
			t.Fatal("selection preparation deadline")
		}
		selected := choice
		if selected < 0 {
			selected = 1
		}
		if h.Observe().FileIndex != selected {
			t.Fatal("selection precedence changed")
		}
		client := &http.Client{Timeout: 3 * time.Second}
		for _, header := range []string{"", "bytes=0-999999", "bytes=-128"} {
			req, _ := http.NewRequest("GET", h.MediaURL(), nil)
			if header != "" {
				req.Header.Set("Range", header)
			}
			response, e := client.Do(req)
			if e != nil {
				t.Fatal(e)
			}
			data, e := io.ReadAll(response.Body)
			response.Body.Close()
			if e != nil {
				t.Fatal(e)
			}
			wanted := contents[selected]
			if header == "bytes=-128" {
				wanted = wanted[len(wanted)-128:]
			}
			if !bytes.Equal(data, wanted) {
				t.Fatal("selected-file byte boundary crossed")
			}
		}
		h.Close()
	}
}

func TestMetadataAndBytesResumeFromMagnetWithoutPeers(t *testing.T) {
	seed, raw, data := seedFixture(t, false)
	root := t.TempDir()
	r, err := newRuntime(Config{CacheDir: root, CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	h := readyHandle(t, r, raw)
	connectSeed(h, seed)
	fetchRange(t, h, 0, 4095)
	hash := h.t.InfoHash().HexString()
	r.Close()
	seed.Close()
	r, err = newRuntime(Config{CacheDir: root, CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	h, err = r.Prepare(context.Background(), Source{Magnet: "magnet:?xt=urn:btih:" + hash}, time.Now().Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}
	select {
	case <-h.done:
	case <-time.After(time.Second):
		t.Fatal("cached magnet metadata required network")
	}
	got := fetchRange(t, h, 0, 4095)
	if !bytes.Equal(got, data[:4096]) {
		t.Fatal("cached magnet payload mismatch")
	}
}

func TestExpectedSourceSizeIsBoundBeforeServingOrArchiveParsing(t *testing.T) {
	_, raw, data := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	index := 0
	for _, delta := range []int64{1, 0} {
		size := int64(len(data)) + delta
		h, err := r.Prepare(context.Background(), Source{Metainfo: raw, FileIndex: &index, ExpectedFileSize: &size}, time.Now().Add(time.Minute))
		if err != nil {
			t.Fatal(err)
		}
		select {
		case <-h.done:
		case <-time.After(time.Second):
			t.Fatal("metadata selection deadline")
		}
		p := h.Observe()
		if delta != 0 {
			if p.Ready || p.Error != "file_index_unavailable" || h.authorized() {
				t.Fatal("mismatched descriptor exposed media")
			}
		} else if !p.Ready || p.Length != size {
			t.Fatal("exact source length refused")
		}
		h.Close()
	}
	invalid := int64(0)
	if h, err := r.Prepare(context.Background(), Source{Metainfo: raw, FileIndex: &index, ExpectedFileSize: &invalid}, time.Now().Add(time.Minute)); err == nil {
		h.Close()
		t.Fatal("invalid source length admitted")
	}
}
