// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"sync/atomic"
	"testing"

	"github.com/anacrolix/torrent/metainfo"
)

// Real bounded storage, including durability work. Run with -mutexprofile and
// -blockprofile; the input is generated and contains no provider media.
func BenchmarkCache(b *testing.B) {
	for _, mixed := range []bool{false, true} {
		name := "verified-read"
		if mixed {
			name = "read-during-completion"
		}
		b.Run(name, func(b *testing.B) {
			c, err := OpenCache(b.TempDir(), 64<<20)
			if err != nil {
				b.Fatal(err)
			}
			defer c.Close()
			info := &metainfo.Info{Name: "generated", Length: 16 << 20, PieceLength: 1 << 20, Pieces: make([]byte, 16*20)}
			s, err := c.OpenTorrent(context.Background(), info, metainfo.Hash{})
			if err != nil {
				b.Fatal(err)
			}
			data := make([]byte, 1<<20)
			for i := 0; i < 16; i++ {
				p := s.Piece(info.Piece(i))
				if _, err := p.WriteAt(data, 0); err != nil {
					b.Fatal(err)
				}
				if err := p.MarkComplete(); err != nil {
					b.Fatal(err)
				}
			}
			var sequence atomic.Uint64
			b.SetBytes(32 << 10)
			b.ResetTimer()
			b.RunParallel(func(pb *testing.PB) {
				buf := make([]byte, 32<<10)
				reader := s.NewReader()
				for pb.Next() {
					n := sequence.Add(1)
					if mixed && n%64 == 0 {
						// One distinct writer per parallel worker is unnecessary: these
						// calls intentionally exercise competing completion operations.
						p := s.Piece(info.Piece(15))
						if err := p.MarkNotComplete(); err != nil {
							b.Error(err)
							return
						}
						if _, err := p.WriteAt(data, 0); err != nil {
							b.Error(err)
							return
						}
						if err := p.MarkComplete(); err != nil {
							b.Error(err)
							return
						}
					} else if _, err := reader.ReadAt(buf, int64(n%15)<<20); err != nil {
						b.Error(err)
						return
					}
				}
			})
		})
	}
}
