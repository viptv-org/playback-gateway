// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"github.com/anacrolix/torrent"
	"sync"
	"time"
)

type requestKey struct{ piece, begin, length uint32 }
type requestObservations struct {
	mu              sync.Mutex
	owners          map[*torrent.Peer]map[requestKey]time.Time
	lastUseful      map[*torrent.Peer]time.Time
	count, overflow int
}

func (o *requestObservations) sent(e torrent.PeerRequestEvent) {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.owners == nil {
		o.owners = map[*torrent.Peer]map[requestKey]time.Time{}
		o.lastUseful = map[*torrent.Peer]time.Time{}
	}
	key := requestKey{uint32(e.Index), uint32(e.Begin), uint32(e.Length)}
	if _, exists := o.owners[e.Peer][key]; !exists {
		if o.count >= 8192 {
			o.overflow++
			return
		}
		if o.owners[e.Peer] == nil {
			o.owners[e.Peer] = map[requestKey]time.Time{}
		}
		o.count++
	}
	o.owners[e.Peer][key] = time.Now()
}
func (o *requestObservations) deleted(e torrent.PeerRequestEvent) {
	o.mu.Lock()
	defer o.mu.Unlock()
	key := requestKey{uint32(e.Index), uint32(e.Begin), uint32(e.Length)}
	if _, ok := o.owners[e.Peer][key]; ok {
		delete(o.owners[e.Peer], key)
		o.count--
	}
}
func (o *requestObservations) received(e torrent.PeerMessageEvent) {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.lastUseful != nil {
		o.lastUseful[e.Peer] = time.Now()
	}
}
func (o *requestObservations) closed(peer *torrent.Peer) {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.count -= len(o.owners[peer])
	delete(o.owners, peer)
	delete(o.lastUseful, peer)
}

type DemandDiagnostics struct {
	HeadPieces              int     `json:"head_pieces"`
	VerifiedHeadPieces      int     `json:"verified_head_pieces"`
	WrittenHeadBytes        int64   `json:"written_head_bytes"`
	HeadBytes               int64   `json:"head_bytes"`
	OutstandingHeadRequests int     `json:"outstanding_head_requests"`
	OldestHeadRequestMS     float64 `json:"oldest_head_request_ms"`
	IdleHeadOwners          int     `json:"idle_head_owners"`
	DroppedObservations     int     `json:"dropped_observations"`
}

func (h *Handle) DemandDiagnostics() DemandDiagnostics {
	var out DemandDiagnostics
	heads := map[int]bool{}
	h.r.mu.Lock()
	for reader, span := range h.r.demands {
		if reader.h == h && span.begin < span.end {
			heads[span.begin] = true
		}
	}
	h.r.mu.Unlock()
	out.HeadPieces = len(heads)
	info := h.t.Info()
	if info == nil {
		return out
	}
	hash := h.t.InfoHash().HexString()
	h.r.cache.mu.Lock()
	for index := range heads {
		out.HeadBytes += info.Piece(index).Length()
		key := pieceKey(hash, index)
		if e := h.r.cache.entries[key]; e != nil {
			if e.complete {
				out.VerifiedHeadPieces++
				out.WrittenHeadBytes += e.length
			} else {
				for _, span := range e.writes {
					out.WrittenHeadBytes += span.end - span.start
				}
			}
		}
	}
	h.r.cache.mu.Unlock()
	o := &h.r.requests
	o.mu.Lock()
	defer o.mu.Unlock()
	out.DroppedObservations = o.overflow
	now := time.Now()
	for peer, requests := range o.owners {
		if peer.Torrent() != h.t {
			continue
		}
		ownsHead := false
		firstOutstanding := now
		for key, sent := range requests {
			if heads[int(key.piece)] {
				ownsHead = true
				if sent.Before(firstOutstanding) {
					firstOutstanding = sent
				}
				out.OutstandingHeadRequests++
				out.OldestHeadRequestMS = max(out.OldestHeadRequestMS, float64(now.Sub(sent).Microseconds())/1000)
			}
		}
		lastProgress := o.lastUseful[peer]
		if firstOutstanding.After(lastProgress) {
			lastProgress = firstOutstanding
		}
		if ownsHead && now.Sub(lastProgress) >= 2*time.Second {
			out.IdleHeadOwners++
		}
	}
	return out
}
