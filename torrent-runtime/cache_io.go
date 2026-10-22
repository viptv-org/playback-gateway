// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"io"
	"os"
)

const maxCachedPieceFiles = 64

// Called with Cache.mu held and returns with it held. Never wait for an entry
// lock while holding Cache.mu: the current IO owner needs it to publish/retire.
func (c *Cache) beginIOLocked(e *cacheEntry, ownerClosed *bool) (*os.File, bool, error) {
	e.pins++
	e.ioPins++
	c.activeIO++
	c.mu.Unlock()
	e.ioMu.Lock()
	c.mu.Lock()
	if c.closed || c.failed || (ownerClosed != nil && *ownerClosed) {
		c.endIOLocked(e, nil, false)
		return nil, false, os.ErrClosed
	}
	if c.entries[e.key] != e || e.evicting {
		c.endIOLocked(e, nil, false)
		return nil, false, io.EOF
	}
	if e.file != nil {
		return e.file, true, nil
	}
	if c.openFiles >= maxCachedPieceFiles {
		var oldest *cacheEntry
		for _, candidate := range c.entries {
			if candidate.file != nil && candidate.ioPins == 0 && (oldest == nil || candidate.used.Before(oldest.used)) {
				oldest = candidate
			}
		}
		if oldest != nil {
			c.closeFileLocked(oldest)
		}
	}
	retained := c.openFiles < maxCachedPieceFiles
	if retained {
		c.openFiles++
	} // Reserve before yielding to another opener.
	c.mu.Unlock()
	f, err := os.OpenFile(c.path(e.key), os.O_RDWR, 0600)
	c.mu.Lock()
	if err != nil {
		if retained {
			c.openFiles--
		}
		c.endIOLocked(e, nil, false)
		return nil, false, c.failLocked()
	}
	if retained {
		e.file = f
	}
	return f, retained, nil
}

func (c *Cache) endIOLocked(e *cacheEntry, f *os.File, retained bool) {
	if f != nil && !retained {
		f.Close()
	}
	e.pins--
	e.ioPins--
	c.activeIO--
	e.ioMu.Unlock()
	c.signalLocked()
}

// The caller owns Cache.mu and has excluded active IO on this entry.
func (c *Cache) closeFileLocked(e *cacheEntry) {
	if e.file != nil {
		e.file.Close()
		e.file = nil
		c.openFiles--
	}
}

// Only pending readers are registered; torrent size does not grow this map.
// Subscribe before checking completion to avoid losing its publication.
func (c *Cache) watchPiece(key string) (<-chan struct{}, func()) {
	c.mu.Lock()
	// A verified hit normally returns immediately. The inventory generation
	// still fences eviction between this check and the read, without allocating
	// a per-piece subscription for every small HTTP copy.
	if e := c.entries[key]; e != nil && e.complete && !e.evicting {
		changed := c.changed
		c.mu.Unlock()
		return changed, func() {}
	}
	w := c.waiters[key]
	if w == nil {
		w = &pieceWaiter{changed: make(chan struct{})}
		c.waiters[key] = w
	}
	w.refs++
	changed := w.changed
	c.mu.Unlock()
	return changed, func() {
		c.mu.Lock()
		w.refs--
		if w.refs == 0 {
			delete(c.waiters, key)
		}
		c.mu.Unlock()
	}
}

func (c *Cache) notifyPieceLocked(key string) {
	if w := c.waiters[key]; w != nil {
		close(w.changed)
		w.changed = make(chan struct{})
	}
}
