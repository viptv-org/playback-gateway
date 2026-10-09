// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"strconv"
	"sync"
	"time"
)

const MaxControlBytes = 6 << 20

// Controller is the private, process-local control endpoint. It is never
// registered with the HTTP media listener. The parent supplies authorization;
// these handles and URLs must not appear in public models or diagnostics.
type Controller struct {
	mu          sync.Mutex
	runtime     *Runtime
	handles     map[string]*Handle
	next        uint64
	quarantined bool
}
type command struct {
	Version         int     `json:"version"`
	Op              string  `json:"op"`
	Config          *Config `json:"config,omitempty"`
	Source          *Source `json:"source,omitempty"`
	Handle          string  `json:"handle,omitempty"`
	AuthorityMillis int64   `json:"authority_ms,omitempty"`
}
type reply struct {
	Version   int       `json:"version"`
	OK        bool      `json:"ok"`
	Error     string    `json:"error,omitempty"`
	Terminate bool      `json:"terminate,omitempty"`
	Handle    string    `json:"handle,omitempty"`
	MediaURL  string    `json:"media_url,omitempty"`
	Progress  *Progress `json:"progress,omitempty"`
}

func (c *Controller) Call(input []byte) []byte {
	r := c.call(input)
	data, _ := json.Marshal(r)
	return data
}
func (c *Controller) call(input []byte) reply {
	r := reply{Version: 2}
	if len(input) > MaxControlBytes {
		r.Error = "control_too_large"
		return r
	}
	var request command
	decoder := json.NewDecoder(bytes.NewReader(input))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&request) != nil || decoder.Decode(new(any)) != io.EOF || request.Version != 2 {
		r.Error = "invalid_control"
		return r
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.quarantined {
		r.Error = "worker_quarantined"
		r.Terminate = true
		return r
	}
	if request.Op == "open" {
		if c.runtime != nil || request.Config == nil || request.Source != nil || request.Handle != "" {
			r.Error = "invalid_open"
			return r
		}
		var err error
		c.runtime, err = New(*request.Config)
		if err != nil {
			r.Error = "runtime_unavailable"
			return r
		}
		c.handles = map[string]*Handle{}
		r.OK = true
		return r
	}
	if c.runtime == nil {
		r.Error = "runtime_unavailable"
		return r
	}
	var err error
	switch request.Op {
	case "prepare":
		// Retired handles remain observable until explicitly closed. Bound those
		// records too, so repeated expired preparations cannot grow the controller.
		if len(c.handles) >= c.runtime.maxHandles {
			r.Error = "handle_limit"
			return r
		}
		if request.Source == nil || request.AuthorityMillis <= 0 || request.AuthorityMillis > 60000 {
			r.Error = "invalid_prepare"
			return r
		}
		var h *Handle
		h, err = c.runtime.Prepare(context.Background(), *request.Source, time.Now().Add(time.Duration(request.AuthorityMillis)*time.Millisecond))
		if err == nil {
			c.next++
			r.Handle = strconv.FormatUint(c.next, 10)
			c.handles[r.Handle] = h
		}
	case "observe", "renew", "first_frame", "close":
		h := c.handles[request.Handle]
		if h == nil {
			r.Error = "handle_unavailable"
			return r
		}
		switch request.Op {
		case "observe":
			p := h.Observe()
			r.Progress = &p
			if p.Ready && h.authorized() {
				r.MediaURL = h.MediaURL()
			}
		case "renew":
			if request.AuthorityMillis <= 0 || request.AuthorityMillis > 60000 {
				err = errors.New("invalid authority")
			} else {
				err = h.Renew(time.Now().Add(time.Duration(request.AuthorityMillis) * time.Millisecond))
			}
		case "first_frame":
			err = h.ReportFirstFrame()
		case "close":
			delete(c.handles, request.Handle)
			err = settleWithin(func() error { h.Close(); return nil })
		}
	case "shutdown":
		err = settleWithin(func() error { return c.runtime.Close() })
		if err == nil {
			c.runtime = nil
			c.handles = nil
		}
	default:
		r.Error = "invalid_operation"
		return r
	}
	if errors.Is(err, errSettlement) {
		c.quarantined = true
		r.Terminate = true
		r.Error = "settlement_timeout"
		return r
	}
	if err != nil {
		r.Error = "operation_refused"
		return r
	}
	r.OK = true
	return r
}

var errSettlement = errors.New("worker settlement timed out")

func settleWithin(work func() error) error {
	done := make(chan error, 1)
	go func() { done <- work() }()
	select {
	case err := <-done:
		return err
	case <-time.After(2 * time.Second):
		return errSettlement
	}
}
