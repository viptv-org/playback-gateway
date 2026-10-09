// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"encoding/binary"
	"net"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"

	"github.com/anacrolix/dht/v2"
	"github.com/anacrolix/dht/v2/krpc"
	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

// The fixture's root returns one independently known compact endpoint. The
// pinned upstream InMemory store keys by IP alone, then decodes the key as
// IP+port; use an independent oracle rather than depending on that store.
type fixturePeerStore struct {
	hash metainfo.Hash
	peer krpc.NodeAddr
}

func (s fixturePeerStore) AddPeer(metainfo.Hash, krpc.NodeAddr) {}
func (s fixturePeerStore) GetPeers(hash metainfo.Hash) []krpc.NodeAddr {
	if hash != s.hash {
		return nil
	}
	return []krpc.NodeAddr{s.peer}
}

func ownedTCPAddress(t *testing.T, client *torrent.Client) *net.TCPAddr {
	t.Helper()
	for _, address := range client.ListenAddrs() {
		if tcp, ok := address.(*net.TCPAddr); ok {
			return tcp
		}
	}
	t.Fatal("owned TCP seeder unavailable")
	return nil
}

func requireDiscoveredMagnet(t *testing.T, r *Runtime, raw, data []byte, trackers []string, discovery torrent.PeerSource) {
	t.Helper()
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	h, err := r.Prepare(context.Background(), Source{Magnet: "magnet:?xt=urn:btih:" + mi.HashInfoBytes().HexString(), Trackers: trackers}, time.Now().Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}
	defer h.Close()
	select {
	case <-h.done:
	case <-time.After(5 * time.Second):
		t.Fatalf("isolated discovery did not resolve metadata: %+v", h.Diagnostics())
	}
	if !h.Observe().Ready {
		t.Fatal("isolated discovery failed")
	}
	if got := fetchRange(t, h, 0, 4095); !bytes.Equal(got, data[:4096]) {
		t.Fatal("discovered peer returned different verified bytes")
	}
	found := false
	for _, peer := range h.t.PeerConns() {
		found = found || peer.Discovery == discovery
	}
	if !found {
		t.Fatal("payload did not arrive through the required discovery mechanism")
	}
}

func TestTrackerOnlyMagnetResolvesMetadataAndVerifiedBytes(t *testing.T) {
	seed, raw, data := seedFixture(t, false)
	address := ownedTCPAddress(t, seed)
	compact := append([]byte(nil), address.IP.To4()...)
	compact = binary.BigEndian.AppendUint16(compact, uint16(address.Port))
	var queries atomic.Int64
	tracker := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		queries.Add(1)
		body, _ := bencode.Marshal(map[string]any{"interval": 300, "peers": string(compact)})
		w.Header().Set("Content-Type", "application/x-bittorrent")
		_, _ = w.Write(body)
	}))
	defer tracker.Close()
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096,
		clientTuning: func(c *torrent.ClientConfig) { c.NoDHT = true; c.DisablePEX = true }}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	requireDiscoveredMagnet(t, r, raw, data, []string{tracker.URL + "/announce"}, torrent.PeerSourceTracker)
	if queries.Load() == 0 {
		t.Fatal("fixture never received a tracker announce")
	}
	if len(r.client.DhtServers()) != 0 {
		t.Fatal("tracker-only fixture accidentally used DHT")
	}
}

func TestDHTOnlyMagnetResolvesMetadataAndVerifiedBytes(t *testing.T) {
	seed, raw, data := seedFixture(t, false)
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	address := ownedTCPAddress(t, seed)
	store := fixturePeerStore{mi.HashInfoBytes(), krpc.NodeAddr{IP: address.IP, Port: address.Port}}
	packet, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer packet.Close()
	var queries atomic.Int64
	node, err := dht.NewServer(&dht.ServerConfig{Conn: packet, NoSecurity: true, PeerStore: store,
		StartingNodes: func() ([]dht.Addr, error) { return nil, nil },
		OnQuery: func(query *krpc.Msg, _ net.Addr) bool {
			if query.Q == "get_peers" {
				queries.Add(1)
			}
			return true
		}})
	if err != nil {
		t.Fatal(err)
	}
	defer node.Close()
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10, ReadaheadBytes: 4096,
		clientTuning: func(c *torrent.ClientConfig) {
			c.NoDHT = false
			c.DisableTrackers = true
			c.DisablePEX = true
			c.DhtStartingNodes = func(string) dht.StartingNodesGetter {
				return func() ([]dht.Addr, error) { return []dht.Addr{dht.NewAddr(packet.LocalAddr())}, nil }
			}
			c.ConfigureAnacrolixDhtServer = func(config *dht.ServerConfig) { config.NoSecurity = true }
		}}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	requireDiscoveredMagnet(t, r, raw, data, nil, torrent.PeerSourceDhtGetPeers)
	if queries.Load() == 0 {
		t.Fatal("fixture never received a DHT peer lookup")
	}
}

func TestUnreachableMetadataPreparationCancelsAndSettles(t *testing.T) {
	_, raw, _ := seedFixture(t, false)
	mi, err := metainfo.Load(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	unused, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := unused.Addr()
	unused.Close()
	r, err := newRuntime(Config{CacheDir: t.TempDir(), CacheBytes: 64 << 10,
		clientTuning: func(c *torrent.ClientConfig) { c.DisableTrackers = true; c.DisablePEX = true; c.DisableUTP = true }}, true)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	h, err := r.Prepare(ctx, Source{Magnet: "magnet:?xt=urn:btih:" + mi.HashInfoBytes().HexString()}, time.Now().Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}
	h.t.AddPeers([]torrent.PeerInfo{{Addr: address}})
	select {
	case <-h.done:
		t.Fatal("unreachable peer unexpectedly supplied metadata")
	case <-time.After(100 * time.Millisecond):
	}
	start := time.Now()
	cancel()
	h.Close()
	select {
	case <-h.done:
	case <-time.After(time.Second):
		t.Fatal("cancelled metadata resolver did not settle")
	}
	if time.Since(start) > 2*time.Second || h.authorized() {
		t.Fatal("cancelled metadata retained activity or authority")
	}
	if h.Observe().Ready {
		t.Fatal("cancelled metadata became ready")
	}
}
