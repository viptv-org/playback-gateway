// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"testing"
	"time"

	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

func TestShutdownRevokesAllHandlesBeforeJoiningBlockedStorage(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	a := readyHandle(t, r, raw)
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	info, err := mi.UnmarshalInfo()
	if err != nil {
		t.Fatal(err)
	}
	info.Name = "independent-shutdown-input.bin"
	mi.InfoBytes, err = bencode.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	var encoded bytes.Buffer
	if err = mi.Write(&encoded); err != nil {
		t.Fatal(err)
	}
	b := readyHandle(t, r, encoded.Bytes())
	r.cache.mu.Lock() // Force whichever storage owner joins first to wait.
	done := make(chan struct{})
	go func() { r.Close(); close(done) }()
	deadline := time.Now().Add(time.Second)
	for (a.authorized() || b.authorized()) && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	allRevoked := !a.authorized() && !b.authorized()
	r.cache.mu.Unlock()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("released shutdown did not settle")
	}
	if !allRevoked {
		t.Fatal("shutdown left another handle authorized while joining blocked storage")
	}
}
