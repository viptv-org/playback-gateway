// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/anacrolix/dht/v2/krpc"
	"github.com/anacrolix/torrent/metainfo"
)

// Reserved record indices cannot collide with a piece index under the 4 MiB
// metainfo bound. Records use the same durable checksums and disk accounting.
const routingRecord = "0000000000000000000000000000000000000000-fffffffe"

func metadataRecord(hash string) string { return hash + "-ffffffff" }

func (c *Cache) readRecord(key string, maxBytes int64) []byte {
	c.mu.Lock()
	defer c.mu.Unlock()
	e := c.entries[key]
	if c.closed || c.failed || e == nil || e.evicting || !e.complete || e.length > maxBytes {
		return nil
	}
	f, err := os.Open(c.path(key))
	if err != nil {
		return nil
	}
	defer f.Close()
	b := make([]byte, e.length)
	if _, err = f.ReadAt(b, pieceHeaderBytes); err != nil {
		return nil
	}
	e.used = time.Now()
	return b
}
func (c *Cache) writeRecord(key string, data []byte) error {
	c.recordMu.Lock()
	defer c.recordMu.Unlock()
	if len(data) == 0 || len(data) > 4<<20 {
		return errors.New("record capacity unavailable")
	}
	c.mu.Lock()
	if c.closed || c.failed {
		c.mu.Unlock()
		return os.ErrClosed
	}
	if e := c.entries[key]; e != nil {
		if e.pins != 0 {
			c.mu.Unlock()
			return errors.New("record still in use")
		}
		if err := os.Remove(c.path(key)); err != nil {
			c.mu.Unlock()
			return errors.New("record replacement failed")
		}
		delete(c.entries, key)
		c.used -= c.charge(e.length)
	}
	c.mu.Unlock()
	closed := false
	p := &cachePiece{c: c, key: key, length: int64(len(data)), ownerClosed: &closed}
	if _, err := p.WriteAt(data, 0); err != nil {
		return err
	}
	return p.MarkComplete()
}

func (r *Runtime) restoreMetainfo(hash string) []byte {
	raw := r.cache.readRecord(metadataRecord(hash), 4<<20)
	if len(raw) == 0 {
		return nil
	}
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil || mi.HashInfoBytes().HexString() != hash {
		return nil
	}
	return mi.InfoBytes
}
func (h *Handle) saveMetainfo() error {
	mi := h.t.Metainfo()
	var b bytes.Buffer
	// Cache immutable info only. Current private descriptors own tracker hints.
	if err := (&metainfo.MetaInfo{InfoBytes: mi.InfoBytes}).Write(&b); err != nil {
		return errors.New("metadata serialization failed")
	}
	return h.r.cache.writeRecord(metadataRecord(h.t.InfoHash().HexString()), b.Bytes())
}

func (r *Runtime) restoreRouting() {
	raw := r.cache.readRecord(routingRecord, 256<<10)
	if len(raw) == 0 {
		return
	}
	var nodes []krpc.NodeInfo
	if json.Unmarshal(raw, &nodes) != nil || len(nodes) > 1024 {
		return
	}
	for _, node := range nodes {
		if !publicIP(node.Addr.IP) || node.Addr.Port <= 0 || node.Addr.Port > 65535 {
			continue
		}
		for _, server := range r.client.DhtServers() {
			server.AddNode(node)
		}
	}
}
func (r *Runtime) saveRouting() {
	var nodes []krpc.NodeInfo
	seen := map[string]bool{}
	for _, server := range r.client.DhtServers() {
		if source, ok := server.(interface{ Nodes() []krpc.NodeInfo }); ok {
			for _, n := range source.Nodes() {
				if !publicIP(n.Addr.IP) || n.Addr.Port <= 0 || n.Addr.Port > 65535 {
					continue
				}
				key := net.JoinHostPort(n.Addr.IP.String(), strconv.Itoa(n.Addr.Port))
				if seen[key] {
					continue
				}
				seen[key] = true
				nodes = append(nodes, n)
				if len(nodes) == 1024 {
					break
				}
			}
		}
		if len(nodes) == 1024 {
			break
		}
	}
	if len(nodes) == 0 {
		return
	}
	data, err := json.Marshal(nodes)
	if err == nil && len(data) <= 256<<10 {
		_ = r.cache.writeRecord(routingRecord, data)
	}
}

// Metadata/routing residency never enters verified payload-progress counters.
func payloadRecord(key string) bool {
	return !strings.HasSuffix(key, "-ffffffff") && key != routingRecord
}

var _ io.ReaderAt = (*verifiedReader)(nil)
