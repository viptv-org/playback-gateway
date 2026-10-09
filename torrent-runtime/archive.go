// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"errors"
	"github.com/anacrolix/torrent"
	"github.com/nwaples/rardecode/v2"
	"io"
	"io/fs"
	"path"
	"regexp"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
)

var multipartName = regexp.MustCompile(`(?i)^(.*)\.part([0-9]+)\.rar$`)
var legacyVolume = regexp.MustCompile(`(?i)^(.*)\.r[0-9]{2}$`)

func archiveName(name string) bool {
	return strings.HasSuffix(strings.ToLower(name), ".rar") || legacyVolume.MatchString(name)
}

type torrentFS struct {
	h            *Handle
	ctx          context.Context
	files        map[string]*torrent.File
	headerBudget atomic.Int64
}

func (f *torrentFS) Open(name string) (fs.File, error) {
	if !fs.ValidPath(name) {
		return nil, fs.ErrInvalid
	}
	file := f.files[name]
	if file == nil {
		return nil, fs.ErrNotExist
	}
	return &volumeReader{demandReader: &demandReader{h: f.h, ctx: f.ctx, file: file}, name: name, fs: f}, nil
}

type volumeReader struct {
	*demandReader
	name string
	fs   *torrentFS
}

func (r *volumeReader) Stat() (fs.FileInfo, error) {
	return volumeInfo{name: r.name, size: r.file.Length()}, nil
}
func (r *volumeReader) Read(b []byte) (int, error) {
	remaining := r.fs.headerBudget.Load()
	if remaining == 0 {
		return 0, errors.New("archive index budget exceeded")
	}
	if remaining > 0 && int64(len(b)) > remaining {
		b = b[:remaining]
	}
	n, err := r.demandReader.Read(b)
	if remaining > 0 {
		r.fs.headerBudget.Add(-int64(n))
	}
	return n, err
}

type volumeInfo struct {
	name string
	size int64
}

func (v volumeInfo) Name() string     { return path.Base(v.name) }
func (v volumeInfo) Size() int64      { return v.size }
func (volumeInfo) Mode() fs.FileMode  { return 0400 }
func (volumeInfo) ModTime() time.Time { return time.Time{} }
func (volumeInfo) IsDir() bool        { return false }
func (volumeInfo) Sys() any           { return nil }

type archiveSelection struct {
	index  int
	length int64
}

func (h *Handle) archiveCatalog(ctx context.Context, selected *torrent.File) ([]*rardecode.File, error) {
	volumes := &torrentFS{h: h, ctx: ctx, files: map[string]*torrent.File{}}
	volumes.headerBudget.Store(4 << 20)
	for _, f := range h.t.Files() {
		name := f.Path()
		if !fs.ValidPath(name) {
			return nil, errors.New("invalid_archive_volume")
		}
		if _, exists := volumes.files[name]; exists {
			return nil, errors.New("invalid_archive_volume")
		}
		volumes.files[name] = f
	}
	first := selected.Path()
	if parts := multipartName.FindStringSubmatch(first); parts != nil {
		found := false
		for name := range volumes.files {
			p := multipartName.FindStringSubmatch(name)
			if p == nil || !strings.EqualFold(p[1], parts[1]) {
				continue
			}
			n, err := strconv.Atoi(p[2])
			if err == nil && n == 1 {
				first = name
				found = true
				break
			}
		}
		if !found {
			return nil, errors.New("archive_volume_missing")
		}
	} else if parts := legacyVolume.FindStringSubmatch(first); parts != nil {
		found := false
		for name := range volumes.files {
			if strings.EqualFold(name, parts[1]+".rar") {
				first = name
				found = true
				break
			}
		}
		if !found {
			return nil, errors.New("archive_volume_missing")
		}
	}
	entries, err := rardecode.List(first, rardecode.FileSystem(volumes), rardecode.BufferSize(4096), rardecode.MaxDictionarySize(64<<20), rardecode.SkipCheck)
	if err != nil {
		if errors.Is(err, rardecode.ErrArchiveEncrypted) || errors.Is(err, rardecode.ErrArchivedFileEncrypted) {
			return nil, errors.New("encrypted_archive_unsupported")
		}
		if errors.Is(err, fs.ErrNotExist) {
			return nil, errors.New("archive_volume_missing")
		}
		return nil, errors.New("archive_metadata_invalid")
	}
	if len(entries) == 0 || len(entries) > 4096 {
		return nil, errors.New("archive_metadata_invalid")
	}
	volumes.headerBudget.Store(-1)
	return entries, nil
}

func (h *Handle) storedArchive(ctx context.Context, volume *torrent.File, index *int) (archiveSelection, error) {
	entries, err := h.archiveCatalog(ctx, volume)
	if err != nil {
		return archiveSelection{}, err
	}
	chosen := 0
	if index != nil {
		chosen = *index
		if chosen < 0 || chosen >= len(entries) {
			return archiveSelection{}, errors.New("archive_index_unavailable")
		}
	} else {
		for i, f := range entries {
			if f.UnPackedSize > entries[chosen].UnPackedSize {
				chosen = i
			}
		}
	}
	reader, err := openArchiveEntry(entries[chosen])
	if err != nil {
		return archiveSelection{}, err
	}
	reader.Close()
	return archiveSelection{index: chosen, length: entries[chosen].UnPackedSize}, nil
}
func openArchiveEntry(file *rardecode.File) (io.ReadSeekCloser, error) {
	if file.Encrypted || file.HeaderEncrypted {
		return nil, errors.New("encrypted_archive_unsupported")
	}
	if file.IsDir || file.LinkType != rardecode.LinkTypeNone || file.UnKnownSize || file.UnPackedSize <= 0 {
		return nil, errors.New("archive_member_unsupported")
	}
	if file.Solid {
		return nil, errors.New("compressed_archive_unsupported")
	}
	reader, err := file.Open()
	if err != nil {
		return nil, errors.New("archive_member_unsupported")
	}
	seekable, ok := reader.(io.ReadSeekCloser)
	if !ok {
		reader.Close()
		return nil, errors.New("compressed_archive_unsupported")
	}
	// SkipCheck allows stored members to remain seekable. Torrent piece hashes
	// authenticate their backing bytes; no full-member RAR checksum is claimed.
	return seekable, nil
}
func (h *Handle) openStoredArchive(ctx context.Context, volume *torrent.File, index int) (io.ReadSeekCloser, error) {
	entries, err := h.archiveCatalog(ctx, volume)
	if err != nil {
		return nil, err
	}
	if index < 0 || index >= len(entries) {
		return nil, errors.New("archive_index_unavailable")
	}
	return openArchiveEntry(entries[index])
}
