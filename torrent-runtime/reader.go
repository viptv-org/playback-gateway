// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"errors"
	"fmt"
	"github.com/anacrolix/torrent"
	"io"
	"time"
)

type pieceWindow struct{ begin, end int }

// demandReader uses upstream piece priorities and completion synchronization,
// but owns its cancellable wait. The pinned upstream Reader recursively retries
// even cancellation errors when Capacity is set in readAt, preventing a
// blocked capped reader from settling. No dependency patch is needed here.
type demandReader struct {
	h                 *Handle
	ctx               context.Context
	file              *torrent.File
	position          int64
	closed            bool
	readAheadOverride int64
	early             bool
}

func (r *demandReader) Read(b []byte) (int, error) {
	if r.closed {
		return 0, errors.New("reader closed")
	}
	if len(b) == 0 {
		return 0, nil
	}
	if r.position >= r.file.Length() {
		return 0, io.EOF
	}
	wanted := min(int64(len(b)), r.file.Length()-r.position)
	b = b[:wanted]
	info := r.h.t.Info()
	absolute := r.file.Offset() + r.position
	readAhead := r.h.r.readAhead
	r.h.mu.Lock()
	if !r.h.firstFrame {
		readAhead = r.h.r.startupReadAhead
	}
	r.h.mu.Unlock()
	if r.readAheadOverride > 0 {
		readAhead = r.readAheadOverride
	}
	span := pieceWindow{int(absolute / info.PieceLength), int((r.file.Offset() + min(r.file.Length(), r.position+max(readAhead, wanted)) + info.PieceLength - 1) / info.PieceLength)}
	if err := r.updateDemand(span); err != nil {
		return 0, err
	}
	reader := verifiedReader{c: r.h.r.cache, hash: r.h.t.InfoHash().HexString(), info: info}
	for {
		if err := r.ctx.Err(); err != nil {
			return 0, err
		}
		if !r.h.authorized() {
			return 0, errors.New("authority unavailable")
		}
		changed, unwatch := r.h.r.cache.watchPiece(pieceKey(reader.hash, int(absolute/info.PieceLength)))
		n, err := reader.readAt(b, absolute, r.early)
		if n > 0 {
			unwatch()
			r.h.recordReadProgress(absolute, absolute+int64(n))
			r.position += int64(n)
			return n, nil
		}
		if err != io.EOF {
			unwatch()
			return 0, err
		}
		// Resynchronize stale upstream have-bits after bounded-store eviction.
		if err = r.refresh(span); err != nil {
			unwatch()
			return 0, err
		}
		select {
		case <-r.ctx.Done():
			unwatch()
			return 0, r.ctx.Err()
		case <-r.h.r.cache.stopped:
			unwatch()
			return 0, ErrCacheUnavailable
		case <-changed:
		}
		unwatch()
	}
}

func (h *Handle) recordReadProgress(start, end int64) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.firstFrame || end <= start {
		return
	}
	for _, span := range h.consumed {
		if span.start <= start && span.end >= end {
			return
		}
	}
	// Bound fragmented startup reads independently from torrent size. Saturating
	// this meter can stop deadline extensions, never confer extra authority.
	if len(h.consumed) >= 4096 {
		return
	}
	h.consumed = mergeSpans(h.consumed, start, end)
	h.progressAt = time.Now()
	h.progressBoot, _ = elapsedNow()
}

func (r *demandReader) refresh(span pieceWindow) error {
	runtime := r.h.r
	runtime.mu.Lock()
	defer runtime.mu.Unlock()
	if runtime.closed || runtime.refs[r.h.t] == 0 || r.ctx.Err() != nil {
		return errors.New("reader retired")
	}
	for i := span.begin; i < span.end; i++ {
		r.h.t.Piece(i).UpdateCompletion()
	}
	return nil
}

func (r *demandReader) updateDemand(span pieceWindow) error {
	runtime := r.h.r
	runtime.mu.Lock()
	defer runtime.mu.Unlock()
	if runtime.closed || runtime.refs[r.h.t] == 0 || r.ctx.Err() != nil {
		return errors.New("reader retired")
	}
	if old, ok := runtime.demands[r]; ok && old == span {
		return nil
	}
	headCost := int64(0)
	payloadCost := int64(0)
	uniqueHeads := map[string]bool{}
	for reader, current := range runtime.demands {
		if reader == r {
			continue
		}
		if current.begin < current.end {
			key := fmt.Sprintf("%s-%d", reader.h.t.InfoHash().HexString(), current.begin)
			if !uniqueHeads[key] {
				uniqueHeads[key] = true
				headCost += runtime.cache.charge(reader.h.t.Info().Piece(current.begin).Length())
				payloadCost += reader.h.t.Info().PieceLength
			}
		}
	}
	if span.begin < span.end {
		key := fmt.Sprintf("%s-%d", r.h.t.InfoHash().HexString(), span.begin)
		if !uniqueHeads[key] {
			headCost += runtime.cache.charge(r.h.t.Info().Piece(span.begin).Length())
			payloadCost += r.h.t.Info().PieceLength
		}
	}
	if !runtime.cache.headsFit(headCost, payloadCost) {
		return ErrCapacity
	}
	if span.begin == span.end {
		delete(runtime.demands, r)
	} else {
		runtime.demands[r] = span
	}
	runtime.reprioritize(r.h.t)
	return nil
}

// Caller holds Runtime.mu. Cache demand changes do not invoke eviction; its
// disk allocation callbacks acquire this mutex only after releasing Cache.mu.
func (r *Runtime) reprioritize(t *torrent.Torrent) {
	next := map[int]torrent.PiecePriority{}
	wanted := map[int]bool{}
	heads := map[int]bool{}
	for reader, span := range r.demands {
		if reader.h.t != t {
			continue
		}
		for i := span.begin; i < span.end; i++ {
			wanted[i] = true
			p := torrent.PiecePriorityReadahead
			if i == span.begin {
				heads[i] = true
				p = torrent.PiecePriorityNow
			}
			if p > next[i] {
				next[i] = p
			}
		}
	}
	r.cache.setDemand(t.InfoHash().HexString(), wanted, heads)
	for i, old := range r.priorities[t] {
		if next[i] != old {
			t.Piece(i).SetPriority(next[i])
		}
	}
	for i, p := range next {
		if r.priorities[t][i] != p {
			t.Piece(i).SetPriority(p)
		}
	}
	r.priorities[t] = next
}

func (r *demandReader) Seek(offset int64, whence int) (int64, error) {
	if r.closed {
		return 0, errors.New("reader closed")
	}
	position := offset
	switch whence {
	case io.SeekStart:
	case io.SeekCurrent:
		position = r.position + offset
	case io.SeekEnd:
		position = r.file.Length() + offset
	default:
		return 0, errors.New("invalid seek")
	}
	if position < 0 {
		return 0, errors.New("invalid seek")
	}
	if err := r.updateDemand(pieceWindow{}); err != nil {
		return 0, err
	}
	r.position = position
	return position, nil
}
func (r *demandReader) Close() error {
	if r.closed {
		return nil
	}
	r.closed = true
	runtime := r.h.r
	runtime.mu.Lock()
	defer runtime.mu.Unlock()
	delete(runtime.demands, r)
	if !runtime.closed && runtime.refs[r.h.t] > 0 {
		runtime.reprioritize(r.h.t)
	}
	return nil
}
