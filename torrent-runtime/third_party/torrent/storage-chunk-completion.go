// SPDX-License-Identifier: MPL-2.0
package torrent

// UpdateStorageAt refreshes completion and repairs received-chunk bits for a
// bounded store that may discard obsolete writes or evict partial pieces.
// Presence is optional and metadata-only. Normal disk stores retain their
// existing behavior. Whole-piece hash verification is unchanged.
func (p Piece) UpdateStorageAt(offset int64, wholePiece bool) (repaired int) {
	t := p.t
	t.cl.lock()
	defer t.cl.unlock()
	if t.closed.IsSet() {
		return
	}
	t.updatePieceCompletion(p.Index())
	if t.pieceComplete(p.Index()) || offset < 0 || offset >= int64(p.length()) {
		return
	}
	presence, ok := p.Storage().PieceImpl.(interface{ HasData(int64, int64) bool })
	if !ok {
		return
	}
	first := chunkIndexType(offset / int64(p.chunkSize()))
	end := first + 1
	if wholePiece {
		first = 0
		end = p.numChunks()
	}
	for chunk := first; chunk < end; chunk++ {
		if !p.chunkIndexDirty(chunk) {
			continue
		}
		spec := p.chunkIndexSpec(chunk)
		if presence.HasData(int64(spec.Begin), int64(spec.Length)) {
			continue
		}
		t.dirtyChunks.Remove(p.requestIndexBegin() + RequestIndex(chunk))
		repaired++
	}
	if repaired > 0 {
		t.updatePiecePriority(p.Index(), "storage chunk missing")
		for conn := range t.conns {
			conn.onNeedUpdateRequests("storage chunk missing")
		}
	}
	return
}
