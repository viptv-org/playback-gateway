// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"context"
	"net"
	"time"

	"github.com/anacrolix/torrent"
)

// Qualification experiment: let TCP establish briefly before racing the same
// owned uTP socket. Its engine still owns listening, DHT and final socket close.
type delayedDialer struct {
	torrent.Dialer
	delay  time.Duration
	closed <-chan struct{}
}

func (d delayedDialer) Dial(ctx context.Context, address string) (net.Conn, error) {
	timer := time.NewTimer(d.delay)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-d.closed:
		return nil, net.ErrClosed
	case <-timer.C:
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		select {
		case <-d.closed:
			return nil, net.ErrClosed
		default:
		}
		return d.Dialer.Dial(ctx, address)
	}
}
