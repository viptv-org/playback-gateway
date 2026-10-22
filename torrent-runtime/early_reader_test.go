// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"io"
	"testing"
	"time"
)

func TestEarlyReadNeverBridgesUnwrittenHolesOrRestoresPartialData(t *testing.T) {
	root := t.TempDir()
	c, err := OpenCache(root, 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	s, info := testStorage(t, c)
	p := s.Piece(info.Piece(0))
	reader := &verifiedReader{c: c, hash: "0000000000000000000000000000000000000000", info: info}
	changed, cancel := c.watchPiece(pieceKey(reader.hash, 0))
	defer cancel()
	if _, err := p.WriteAt(bytes.Repeat([]byte{7}, 16), 32); err != nil {
		t.Fatal(err)
	}
	select {
	case <-changed:
	case <-time.After(time.Second):
		t.Fatal("block arrival did not wake a pending reader")
	}
	if n, err := reader.readAt(make([]byte, 48), 0, true); n != 0 || err != io.EOF {
		t.Fatal("unwritten prefix escaped")
	}
	if _, err := p.WriteAt(bytes.Repeat([]byte{9}, 16), 0); err != nil {
		t.Fatal(err)
	}
	b := make([]byte, 48)
	if n, err := reader.readAt(b, 0, true); n != 16 || err != io.EOF || !bytes.Equal(b[:16], bytes.Repeat([]byte{9}, 16)) {
		t.Fatal("read crossed a hole or changed received bytes", n, err)
	}
	if n, _ := reader.ReadAt(b, 0); n != 0 {
		t.Fatal("verified reader exposed a partial piece")
	}
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	c, err = OpenCache(root, 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, _ = testStorage(t, c)
	if n, _ := s.NewReader().ReadAt(b, 0); n != 0 {
		t.Fatal("restart trusted unverified data")
	}
}

func TestRejectedEarlyBytesRequireVerificationBeforeFurtherReads(t *testing.T) {
	c, err := OpenCache(t.TempDir(), 64<<10)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	s, info := testStorage(t, c)
	p := s.Piece(info.Piece(0))
	reader := &verifiedReader{c: c, hash: "0000000000000000000000000000000000000000", info: info}
	bad := bytes.Repeat([]byte{99}, 1024)
	if _, err := p.WriteAt(bad, 0); err != nil {
		t.Fatal(err)
	}
	b := make([]byte, 16)
	if n, err := reader.readAt(b, 0, true); n != len(b) || err != nil {
		t.Fatal("early bytes were unnecessarily delayed")
	}
	if err := p.MarkNotComplete(); err != nil {
		t.Fatal(err)
	}
	good := bytes.Repeat([]byte{7}, 1024)
	if _, err := p.WriteAt(good, 0); err != nil {
		t.Fatal(err)
	}
	if n, _ := reader.readAt(b, 0, true); n != 0 {
		t.Fatal("hash rejection allowed another unverified generation")
	}
	if err := p.MarkComplete(); err != nil {
		t.Fatal(err)
	}
	if n, err := reader.ReadAt(b, 0); n != len(b) || err != nil || !bytes.Equal(b, good[:len(b)]) {
		t.Fatal("verified redownload did not recover")
	}
}
