// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/storage"
)

const pieceHeaderBytes = 64

func pieceKey(hash string, index int) string { return fmt.Sprintf("%s-%08x", hash, index) }

var ErrCapacity = errors.New("torrent cache capacity unavailable")

// Cache owns a single namespace. Its lock serializes mutations with reads so a
// slot cannot disappear or be overwritten during an actual disk operation.
// Piece readers additionally retain a pin across hashing's multiple reads.
type Cache struct {
	mu              sync.Mutex
	recordMu        sync.Mutex
	root            string
	limit, used     int64
	allocationUnit  int64
	entries         map[string]*cacheEntry
	lock            *os.File
	closed          bool
	changed         chan struct{}
	streaming       bool
	wanted          map[string]bool
	protected       map[string]bool
	factors         map[*bool]int64
	payloadCapacity atomic.Int64
	capacity        func() (int64, bool)
}

type cacheEntry struct {
	key       string
	length    int64
	complete  bool
	pins      int
	verifying bool
	used      time.Time
	writes    []byteSpan
}

type byteSpan struct{ start, end int64 }

func (e *cacheEntry) written(start, end int64) bool {
	if e.complete {
		return true
	}
	for _, span := range e.writes {
		if span.start <= start && span.end >= end {
			return true
		}
	}
	return false
}
func (e *cacheEntry) recordWrite(start, end int64) {
	e.writes = mergeSpans(e.writes, start, end)
}

func mergeSpans(existing []byteSpan, start, end int64) []byteSpan {
	spans := append(existing, byteSpan{start, end})
	sort.Slice(spans, func(i, j int) bool { return spans[i].start < spans[j].start })
	var merged []byteSpan
	for _, span := range spans {
		n := len(merged)
		if n > 0 && merged[n-1].end >= span.start {
			merged[n-1].end = max(merged[n-1].end, span.end)
		} else {
			merged = append(merged, span)
		}
	}
	return merged
}

type cachePiece struct {
	c           *Cache
	key         string
	length      int64
	ownerClosed *bool
}

type CacheStats struct {
	UsedBytes  int64 `json:"used_bytes"`
	LimitBytes int64 `json:"limit_bytes"`
	Pieces     int   `json:"pieces"`
}

func OpenCache(root string, limit int64) (*Cache, error) {
	if limit <= pieceHeaderBytes {
		return nil, ErrCapacity
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, errors.New("cache directory unavailable")
	}
	if st, err := os.Lstat(root); err != nil || !st.IsDir() || st.Mode()&os.ModeSymlink != 0 {
		return nil, errors.New("invalid cache directory")
	}
	f, err := os.OpenFile(filepath.Join(root, ".lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, errors.New("cache lock unavailable")
	}
	if err = lockCache(f); err != nil {
		f.Close()
		return nil, errors.New("cache already owned")
	}
	unit, _, err := diskGeometry(root)
	if err != nil || unit <= 0 {
		f.Close()
		return nil, errors.New("cache disk geometry unavailable")
	}
	c := &Cache{root: root, limit: limit, allocationUnit: unit, entries: make(map[string]*cacheEntry), lock: f, changed: make(chan struct{}), wanted: map[string]bool{}, protected: map[string]bool{}, factors: map[*bool]int64{}}
	if err = c.restore(); err != nil {
		c.Close()
		return nil, err
	}
	_, free, err := diskGeometry(root)
	if err != nil {
		c.Close()
		return nil, errors.New("cache space unavailable")
	}
	c.limit = min(limit, max(0, free+c.used-(512<<20)))
	if c.limit < unit*2 {
		c.Close()
		return nil, ErrCapacity
	}
	if err = c.reserveLocked(0, ""); err != nil {
		c.Close()
		return nil, err
	}
	c.updateCapacityLocked()
	c.capacity = func() (int64, bool) { return c.payloadCapacity.Load(), true }
	return c, nil
}

func (c *Cache) charge(length int64) int64 {
	return ((length + pieceHeaderBytes + c.allocationUnit - 1) / c.allocationUnit) * c.allocationUnit
}

func (c *Cache) updateCapacityLocked() {
	factor := int64(1)
	for _, value := range c.factors {
		factor = max(factor, value)
	}
	c.payloadCapacity.Store((c.limit - min(int64(64<<20), c.limit/4)) / factor)
}

// Reader heads must fit both actual allocation and the conservative payload
// window advertised to the engine. Complete protected heads also occupy that
// window, so admitting beyond it can starve an otherwise healthy later reader.
func (c *Cache) headsFit(physical, payload int64) bool {
	return physical <= c.limit-min(int64(64<<20), c.limit/4) && payload <= c.payloadCapacity.Load()
}

func (c *Cache) setDemand(hash string, wanted, heads map[int]bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.streaming = true
	for key := range c.wanted {
		if strings.HasPrefix(key, hash+"-") {
			delete(c.wanted, key)
			delete(c.protected, key)
		}
	}
	for index := range wanted {
		key := fmt.Sprintf("%s-%08x", hash, index)
		c.wanted[key] = true
		c.protected[key] = heads[index]
	}
}

func (c *Cache) restore() error {
	files, err := os.ReadDir(c.root)
	if err != nil {
		return errors.New("cache inventory unavailable")
	}
	for _, f := range files {
		if f.Name() == ".lock" {
			continue
		}
		// This directory belongs exclusively to this cache, but unknown files
		// are not deleted. Refuse rather than undercount their storage.
		name := strings.TrimSuffix(f.Name(), ".piece")
		parts := strings.Split(name, "-")
		if len(parts) != 2 || len(parts[0]) != 40 || len(parts[1]) != 8 || !strings.HasSuffix(f.Name(), ".piece") || f.Type()&os.ModeSymlink != 0 || f.IsDir() {
			return errors.New("unexpected cache entry")
		}
		if _, err = hex.DecodeString(parts[0] + parts[1]); err != nil {
			return errors.New("invalid cache entry")
		}
		file, e := os.Open(filepath.Join(c.root, f.Name()))
		if e != nil {
			return errors.New("cache entry unavailable")
		}
		var h [pieceHeaderBytes]byte
		_, e = io.ReadFull(file, h[:])
		st, se := file.Stat()
		length := int64(binary.LittleEndian.Uint64(h[8:16]))
		valid := e == nil && se == nil && string(h[:8]) == "VTRPC001" && length > 0 && st.Size() == length+pieceHeaderBytes
		complete := false
		if valid && h[16] == 1 {
			digest := sha256.New()
			_, e = io.Copy(digest, file)
			complete = e == nil && string(digest.Sum(nil)) == string(h[32:64])
		}
		file.Close()
		// Interrupted and damaged writes never restore completion authority.
		if !valid || !complete {
			if e = os.Remove(filepath.Join(c.root, f.Name())); e != nil {
				return errors.New("damaged cache entry cannot be retired")
			}
			continue
		}
		c.entries[name] = &cacheEntry{key: name, length: length, complete: true, used: st.ModTime()}
		c.used += c.charge(length)
	}
	return c.reserveLocked(0, "")
}

func (c *Cache) Stats() CacheStats {
	c.mu.Lock()
	defer c.mu.Unlock()
	return CacheStats{c.used, c.limit, len(c.entries)}
}

func (c *Cache) OpenTorrent(_ context.Context, info *metainfo.Info, hash metainfo.Hash) (storage.TorrentImpl, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return storage.TorrentImpl{}, os.ErrClosed
	}
	if info.PieceLength <= 0 || info.PieceLength > c.limit/2 || c.charge(info.PieceLength) > c.limit/2 {
		return storage.TorrentImpl{}, ErrCapacity
	}
	closed := false
	factor := (c.charge(info.PieceLength) + info.PieceLength - 1) / info.PieceLength
	currentFactor := int64(1)
	for _, existing := range c.factors {
		currentFactor = max(currentFactor, existing)
	}
	if factor > currentFactor && len(c.protected) > 0 {
		return storage.TorrentImpl{}, ErrCapacity
	}
	c.factors[&closed] = factor
	c.updateCapacityLocked()
	return storage.TorrentImpl{Piece: func(p metainfo.Piece) storage.PieceImpl {
		return &cachePiece{c, fmt.Sprintf("%s-%08x", hash.HexString(), p.Index()), p.Length(), &closed}
	}, Capacity: &c.capacity, NewReader: func() storage.TorrentReader { return &verifiedReader{c: c, hash: hash.HexString(), info: info} }, Close: func() error {
		c.mu.Lock()
		defer c.mu.Unlock()
		closed = true
		delete(c.factors, &closed)
		c.updateCapacityLocked()
		for key := range c.wanted {
			if strings.HasPrefix(key, hash.HexString()+"-") {
				delete(c.wanted, key)
				delete(c.protected, key)
			}
		}
		for key, e := range c.entries {
			if strings.HasPrefix(key, hash.HexString()+"-") {
				if e.pins != 0 {
					return errors.New("cache owner still has pinned IO")
				}
				e.verifying = false
				if e.complete {
					continue
				}
				if err := os.Remove(c.path(key)); err != nil {
					return errors.New("partial cache retirement failed")
				}
				delete(c.entries, key)
				c.used -= c.charge(e.length)
			}
		}
		return nil
	}}, nil
}

// Application readers never share the hasher's incomplete-data read interface.
// An engine completion bit can lag an eviction; checking under the cache lock
// prevents a newly allocated, partially written slot from leaking through it.
type verifiedReader struct {
	c    *Cache
	hash string
	info *metainfo.Info
}

func (r *verifiedReader) Close() error { return nil }
func (r *verifiedReader) ReadAt(b []byte, off int64) (int, error) {
	r.c.mu.Lock()
	defer r.c.mu.Unlock()
	if r.c.closed {
		return 0, os.ErrClosed
	}
	if off < 0 {
		return 0, os.ErrInvalid
	}
	n := 0
	for n < len(b) {
		index := off / r.info.PieceLength
		relative := off % r.info.PieceLength
		key := fmt.Sprintf("%s-%08x", r.hash, index)
		e := r.c.entries[key]
		if e == nil || !e.complete || relative >= e.length {
			return n, io.EOF
		}
		count := min(int64(len(b)-n), e.length-relative)
		f, err := os.Open(r.c.path(key))
		if err != nil {
			return n, io.EOF
		}
		got, err := f.ReadAt(b[n:n+int(count)], relative+pieceHeaderBytes)
		f.Close()
		e.used = time.Now()
		n += got
		off += int64(got)
		if err != nil {
			return n, err
		}
		if got == 0 {
			return n, io.EOF
		}
	}
	return n, nil
}

func (c *Cache) reserveLocked(extra int64, except string) error {
	if extra < 0 || extra > c.limit {
		return ErrCapacity
	}
	if c.used+extra <= c.limit {
		return nil
	}
	entries := make([]*cacheEntry, 0, len(c.entries))
	for _, e := range c.entries {
		if e.key != except && e.pins == 0 && !e.verifying && !c.protected[e.key] {
			entries = append(entries, e)
		}
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].used.Before(entries[j].used) })
	for _, e := range entries {
		if err := os.Remove(c.path(e.key)); err != nil {
			return errors.New("cache eviction failed")
		}
		delete(c.entries, e.key)
		c.used -= c.charge(e.length)
		if c.used+extra <= c.limit {
			return nil
		}
	}
	return ErrCapacity
}

func (c *Cache) path(key string) string { return filepath.Join(c.root, key+".piece") }

func (p *cachePiece) WriteAt(b []byte, off int64) (int, error) {
	c := p.c
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed || *p.ownerClosed {
		return 0, os.ErrClosed
	}
	if off < 0 || off > p.length || int64(len(b)) > p.length-off {
		return 0, errors.New("invalid piece write")
	}
	// Late network chunks after a seek have no current demand. Treat their
	// data as immediately evicted; completion stays false and a hash read sees
	// EOF rather than fabricated zero-filled holes. Upstream explicitly treats
	// storage IO loss differently from peer-supplied corrupt data.
	if c.streaming && payloadRecord(p.key) && !c.wanted[p.key] {
		return len(b), nil
	}
	e := c.entries[p.key]
	if e == nil {
		if err := c.reserveLocked(c.charge(p.length), p.key); err != nil {
			return 0, err
		}
		f, err := os.OpenFile(c.path(p.key), os.O_CREATE|os.O_EXCL|os.O_RDWR, 0600)
		if err != nil {
			return 0, errors.New("cache piece allocation failed")
		}
		var h [pieceHeaderBytes]byte
		copy(h[:8], "VTRPC001")
		binary.LittleEndian.PutUint64(h[8:16], uint64(p.length))
		_, err = f.Write(h[:])
		if err == nil {
			err = f.Truncate(p.length + pieceHeaderBytes)
		}
		ce := f.Close()
		if err == nil {
			err = ce
		}
		if err != nil {
			os.Remove(c.path(p.key))
			return 0, errors.New("cache piece allocation failed")
		}
		e = &cacheEntry{key: p.key, length: p.length, used: time.Now()}
		c.entries[p.key] = e
		c.used += c.charge(p.length)
	}
	if e.complete {
		return 0, errors.New("cannot overwrite verified piece")
	}
	f, err := os.OpenFile(c.path(p.key), os.O_RDWR, 0600)
	if err != nil {
		return 0, errors.New("cache piece unavailable")
	}
	defer f.Close()
	n, err := f.WriteAt(b, off+pieceHeaderBytes)
	if n > 0 {
		e.recordWrite(off, off+int64(n))
	}
	e.used = time.Now()
	return n, err
}

func (p *cachePiece) ReadAt(b []byte, off int64) (int, error) {
	c := p.c
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed || *p.ownerClosed {
		return 0, os.ErrClosed
	}
	e := c.entries[p.key]
	if e == nil {
		return 0, io.EOF
	}
	if off < 0 {
		return 0, os.ErrInvalid
	}
	if off >= p.length {
		return 0, io.EOF
	}
	if int64(len(b)) > p.length-off {
		b = b[:p.length-off]
	}
	if !e.written(off, off+int64(len(b))) {
		return 0, io.EOF
	}
	f, err := os.Open(c.path(p.key))
	if err != nil {
		return 0, io.EOF
	}
	defer f.Close()
	e.used = time.Now()
	return f.ReadAt(b, off+pieceHeaderBytes)
}

func (p *cachePiece) Completion() storage.Completion {
	p.c.mu.Lock()
	defer p.c.mu.Unlock()
	e := p.c.entries[p.key]
	return storage.Completion{Ok: true, Complete: !p.c.closed && !*p.ownerClosed && e != nil && e.complete}
}

func (p *cachePiece) MarkComplete() error {
	c := p.c
	c.mu.Lock()
	defer c.mu.Unlock()
	e := c.entries[p.key]
	if c.closed || *p.ownerClosed {
		return os.ErrClosed
	}
	if e == nil {
		return io.EOF
	}
	defer func() { e.verifying = false }()
	if e.complete {
		return nil
	}
	f, err := os.OpenFile(c.path(p.key), os.O_RDWR, 0600)
	if err != nil {
		return errors.New("cache completion unavailable")
	}
	defer f.Close()
	digest := sha256.New()
	if _, err = io.Copy(digest, io.NewSectionReader(f, pieceHeaderBytes, p.length)); err != nil {
		return err
	}
	if _, err = f.WriteAt(digest.Sum(nil), 32); err != nil {
		return err
	}
	if err = f.Sync(); err != nil {
		return err
	}
	if _, err = f.WriteAt([]byte{1}, 16); err != nil {
		return err
	}
	if err = f.Sync(); err != nil {
		return err
	}
	e.complete = true
	e.writes = nil
	e.used = time.Now()
	close(c.changed)
	c.changed = make(chan struct{})
	return nil
}

func (p *cachePiece) MarkNotComplete() error {
	c := p.c
	c.mu.Lock()
	defer c.mu.Unlock()
	e := c.entries[p.key]
	if c.closed || *p.ownerClosed {
		return os.ErrClosed
	}
	if e == nil {
		return nil
	}
	e.verifying = false
	e.complete = false
	e.writes = nil
	f, err := os.OpenFile(c.path(p.key), os.O_RDWR, 0600)
	if err != nil {
		return errors.New("cache invalidation unavailable")
	}
	defer f.Close()
	_, err = f.WriteAt([]byte{0}, 16)
	return err
}

type pinnedReader struct {
	p    *cachePiece
	e    *cacheEntry
	once sync.Once
}

func (r *pinnedReader) ReadAt(b []byte, off int64) (int, error) { return r.p.ReadAt(b, off) }
func (r *pinnedReader) Close() error {
	r.once.Do(func() { r.p.c.mu.Lock(); r.e.pins--; r.p.c.mu.Unlock() })
	return nil
}
func (p *cachePiece) NewReader() (storage.PieceReader, error) {
	p.c.mu.Lock()
	defer p.c.mu.Unlock()
	e := p.c.entries[p.key]
	if p.c.closed || *p.ownerClosed {
		return nil, os.ErrClosed
	}
	if e == nil {
		return nil, io.EOF
	}
	e.pins++
	// In the supported v1 engine this interface is used for piece hashing.
	// Closing the reader ends IO, but hashing publishes its verdict afterward.
	// Retain eviction protection through MarkComplete/MarkNotComplete (or the
	// joined storage Close when the torrent is dropped).
	e.verifying = true
	return &pinnedReader{p: p, e: e}, nil
}

func (c *Cache) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil
	}
	c.closed = true
	return c.lock.Close()
}
