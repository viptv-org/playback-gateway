// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"crypto/sha1"
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
	"golang.org/x/time/rate"
)

// This opt-in fixture isolates request scheduling from discovery, public peers,
// media parsing and GUI launch. Each request incurs the same response latency;
// responses can overlap in flight but share one bounded upload rate.
func TestPipelineQualification(t *testing.T) {
	if os.Getenv("TORRENT_PIPELINE_FIXTURE") != "1" {
		t.Skip("explicit controlled request-pipeline experiment only")
	}
	const pieceBytes = 2 << 20
	data := make([]byte, 2*pieceBytes)
	for i := range data {
		data[i] = byte(i*31 + i/123)
	}
	var hashes []byte
	for off := 0; off < len(data); off += pieceBytes {
		hash := sha1.Sum(data[off : off+pieceBytes])
		hashes = append(hashes, hash[:]...)
	}
	encoded, err := bencode.Marshal(metainfo.Info{Name: "owned-pipeline.bin", PieceLength: pieceBytes, Length: int64(len(data)), Pieces: hashes})
	if err != nil {
		t.Fatal(err)
	}
	mi := &metainfo.MetaInfo{InfoBytes: encoded}
	var raw bytes.Buffer
	if err = mi.Write(&raw); err != nil {
		t.Fatal(err)
	}
	delayMS := int64(120)
	if value := os.Getenv("TORRENT_PIPELINE_DELAY_MS"); value != "" {
		delayMS, err = strconv.ParseInt(value, 10, 32)
		if err != nil || delayMS < 0 || delayMS > 1000 {
			t.Fatal("invalid fixture latency")
		}
	}
	peer := startPipelinePeer(t, data, mi.HashInfoBytes(), pieceBytes, time.Duration(delayMS)*time.Millisecond)
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 32 << 20, ReadaheadBytes: 4 << 20,
		clientTuning: func(c *torrent.ClientConfig) {
			c.HeaderObfuscationPolicy.Preferred = false
			c.DisableUTP = true
			c.DisableTrackers = true
			c.DisablePEX = true
		}}, true)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { r.Close() })
	h := readyHandle(t, r, raw.Bytes())
	h.t.AddPeers([]torrent.PeerInfo{{Addr: peer.listener.Addr()}})
	start := time.Now()
	body := fetchRange(t, h, 0, int64(len(data)-1))
	elapsed := time.Since(start)
	if !bytes.Equal(body, data) {
		t.Fatal("pipeline delivered different or incomplete payload")
	}
	deadline := time.Now().Add(2 * time.Second)
	for h.t.Stats().PiecesComplete != 2 {
		if time.Now().After(deadline) {
			t.Fatal("received pieces did not verify")
		}
		time.Sleep(time.Millisecond)
	}
	result, _ := json.Marshal(map[string]any{
		"elapsed_ms": float64(elapsed.Microseconds()) / 1000, "payload_bytes": len(body),
		"request_latency_ms": delayMS, "upload_bytes_per_second": 4 << 20,
		"requests": peer.requests.Load(), "peak_outstanding": peer.peak.Load(),
		"requests_before_first_response_write": peer.beforeFirst.Load(),
		"verified_pieces":                      h.t.Stats().PiecesComplete, "payload_equal": true,
	})
	t.Log("PIPELINE " + string(result))
}

type pipelinePeer struct {
	listener        net.Listener
	requests        atomic.Int64
	pending         atomic.Int64
	peak            atomic.Int64
	beforeFirst     atomic.Int64
	responseStarted atomic.Bool
}

func startPipelinePeer(t *testing.T, data []byte, hash metainfo.Hash, pieceBytes int, latency time.Duration) *pipelinePeer {
	t.Helper()
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	peer := &pipelinePeer{listener: listener}
	ctx, cancel := context.WithCancel(context.Background())
	var joined sync.WaitGroup
	joined.Add(1)
	go func() {
		defer joined.Done()
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			joined.Add(1)
			go func() {
				defer joined.Done()
				defer conn.Close()
				stop := context.AfterFunc(ctx, func() { conn.Close() })
				defer stop()
				_ = conn.SetDeadline(time.Now().Add(20 * time.Second))
				var handshake [68]byte
				if _, err := io.ReadFull(conn, handshake[:]); err != nil || handshake[0] != 19 || string(handshake[1:20]) != "BitTorrent protocol" || !bytes.Equal(handshake[28:48], hash[:]) {
					return
				}
				clear(handshake[20:28])
				copy(handshake[48:], []byte("-VQ0001-controlled00"))
				if _, err := conn.Write(handshake[:]); err != nil {
					return
				}
				if wireMessage(conn, 5, []byte{0xc0}) != nil || wireMessage(conn, 1, nil) != nil {
					return
				}
				limiter := rate.NewLimiter(4<<20, 16<<10)
				var writing sync.Mutex
				var responses sync.WaitGroup
				defer responses.Wait()
				for {
					var header [4]byte
					if _, err := io.ReadFull(conn, header[:]); err != nil {
						return
					}
					length := binary.BigEndian.Uint32(header[:])
					if length == 0 {
						continue
					}
					if length > 64<<10 {
						return
					}
					message := make([]byte, length)
					if _, err := io.ReadFull(conn, message); err != nil {
						return
					}
					if message[0] != 6 || len(message) != 13 {
						continue
					}
					index := int(binary.BigEndian.Uint32(message[1:5]))
					begin := int(binary.BigEndian.Uint32(message[5:9]))
					count := int(binary.BigEndian.Uint32(message[9:13]))
					if index < 0 || index >= 2 || begin < 0 || count < 1 || count > 16<<10 || begin+count > pieceBytes {
						return
					}
					peer.requests.Add(1)
					if !peer.responseStarted.Load() {
						peer.beforeFirst.Add(1)
					}
					now := peer.pending.Add(1)
					for old := peer.peak.Load(); now > old && !peer.peak.CompareAndSwap(old, now); old = peer.peak.Load() {
					}
					responses.Add(1)
					go func(message []byte, index, begin, count int) {
						defer responses.Done()
						defer peer.pending.Add(-1)
						timer := time.NewTimer(latency)
						defer timer.Stop()
						select {
						case <-ctx.Done():
							return
						case <-timer.C:
						}
						if limiter.WaitN(ctx, count) != nil {
							return
						}
						payload := make([]byte, 8+count)
						copy(payload[:8], message[1:9])
						copy(payload[8:], data[index*pieceBytes+begin:index*pieceBytes+begin+count])
						writing.Lock()
						peer.responseStarted.Store(true)
						_ = wireMessage(conn, 7, payload)
						writing.Unlock()
					}(message, index, begin, count)
				}
			}()
		}
	}()
	t.Cleanup(func() { cancel(); listener.Close(); joined.Wait() })
	return peer
}
