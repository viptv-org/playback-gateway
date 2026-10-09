// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"encoding/hex"
	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	pp "github.com/anacrolix/torrent/peer_protocol"
	"net"
	"sync"
	"time"
)

// Dependency callbacks may hold its client lock. They only touch this separate
// meter, never Runtime.mu, cache IO or another client method requiring that lock.
type sourceProgress struct {
	mu             sync.Mutex
	pieces         map[int]bool
	last           time.Time
	firstMetadata  time.Time
	firstHandshake time.Time
	firstTracker   time.Time
	trackerSuccess int
	trackerErrors  int
}

func (r *Runtime) meter(hash torrent.InfoHash) *sourceProgress {
	value, _ := r.metadataProgress.LoadOrStore(hash, &sourceProgress{pieces: map[int]bool{}})
	return value.(*sourceProgress)
}

func (r *Runtime) handshake(_ *torrent.PeerConn, hash torrent.InfoHash) {
	meter := r.meter(hash)
	meter.mu.Lock()
	defer meter.mu.Unlock()
	if meter.firstHandshake.IsZero() {
		meter.firstHandshake = time.Now()
	}
}
func (r *Runtime) trackerEvent(event torrent.StatusUpdatedEvent) {
	if event.Event != torrent.TrackerAnnounceSuccessful && event.Event != torrent.TrackerAnnounceError {
		return
	}
	value, err := hex.DecodeString(event.InfoHash)
	if err != nil || len(value) != 20 {
		return
	}
	var hash torrent.InfoHash
	copy(hash[:], value)
	meter := r.meter(hash)
	meter.mu.Lock()
	defer meter.mu.Unlock()
	if event.Event == torrent.TrackerAnnounceSuccessful {
		meter.trackerSuccess++
		if meter.firstTracker.IsZero() {
			meter.firstTracker = time.Now()
		}
	} else {
		meter.trackerErrors++
	}
}

func (r *Runtime) metadataMessage(event torrent.PeerConnReadExtensionMessageEvent) {
	name, _, err := event.PeerConn.LocalLtepProtocolMap.LookupId(event.ExtensionNumber)
	if err != nil || name != pp.ExtensionNameMetadata || len(event.Payload) > 17<<10 {
		return
	}
	var header struct {
		Type  int `bencode:"msg_type"`
		Piece int `bencode:"piece"`
		Total int `bencode:"total_size"`
	}
	decoder := bencode.NewDecoder(bytes.NewReader(event.Payload))
	if decoder.Decode(&header) != nil || header.Type != 1 || header.Total <= 0 || header.Total > 4<<20 || header.Piece < 0 || header.Piece >= (header.Total+16383)/16384 {
		return
	}
	t := event.PeerConn.Torrent()
	if t == nil {
		return
	}
	key := t.InfoHash()
	value, _ := r.metadataProgress.LoadOrStore(key, &sourceProgress{pieces: map[int]bool{}})
	meter := value.(*sourceProgress)
	meter.mu.Lock()
	defer meter.mu.Unlock()
	if !meter.pieces[header.Piece] {
		meter.pieces[header.Piece] = true
		meter.last = time.Now()
		if meter.firstMetadata.IsZero() {
			meter.firstMetadata = meter.last
		}
	}
}

type Diagnostics struct {
	KnownPeers       int     `json:"known_peers"`
	PendingPeers     int     `json:"pending_peers"`
	HalfOpenPeers    int     `json:"half_open_peers"`
	ConnectedPeers   int     `json:"connected_peers"`
	MetadataChunks   int     `json:"metadata_chunks"`
	TrackerSuccess   int     `json:"tracker_success"`
	TrackerErrors    int     `json:"tracker_errors"`
	FirstTrackerMS   float64 `json:"first_tracker_ms"`
	FirstHandshakeMS float64 `json:"first_handshake_ms"`
	FirstMetadataMS  float64 `json:"first_metadata_ms"`
	KnownIPv4        int     `json:"known_ipv4"`
	KnownIPv6        int     `json:"known_ipv6"`
}

func (h *Handle) Diagnostics() Diagnostics {
	stats := h.t.Stats()
	out := Diagnostics{KnownPeers: stats.TotalPeers, PendingPeers: stats.PendingPeers, HalfOpenPeers: stats.HalfOpenPeers, ConnectedPeers: stats.ActivePeers, FirstTrackerMS: -1, FirstHandshakeMS: -1, FirstMetadataMS: -1}
	for _, peer := range h.t.KnownSwarm() {
		host, _, err := net.SplitHostPort(peer.Addr.String())
		if err != nil {
			continue
		}
		ip := net.ParseIP(host)
		if ip == nil {
			continue
		}
		if ip.To4() != nil {
			out.KnownIPv4++
		} else {
			out.KnownIPv6++
		}
	}
	value, ok := h.r.metadataProgress.Load(h.t.InfoHash())
	if !ok {
		return out
	}
	meter := value.(*sourceProgress)
	meter.mu.Lock()
	defer meter.mu.Unlock()
	elapsed := func(value time.Time) float64 {
		if value.IsZero() {
			return -1
		}
		return max(0, float64(value.Sub(h.started).Microseconds())/1000)
	}
	out.MetadataChunks = len(meter.pieces)
	out.TrackerSuccess = meter.trackerSuccess
	out.TrackerErrors = meter.trackerErrors
	out.FirstTrackerMS = elapsed(meter.firstTracker)
	out.FirstHandshakeMS = elapsed(meter.firstHandshake)
	out.FirstMetadataMS = elapsed(meter.firstMetadata)
	return out
}

func (r *Runtime) metadataProgressAt(t *torrent.Torrent) time.Time {
	value, ok := r.metadataProgress.Load(t.InfoHash())
	if !ok {
		return time.Time{}
	}
	meter := value.(*sourceProgress)
	meter.mu.Lock()
	defer meter.mu.Unlock()
	return meter.last
}
