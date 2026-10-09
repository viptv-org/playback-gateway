// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"bufio"
	"context"
	"errors"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// This is a decoder pacing probe, not a measurement of a player's buffer or
// displayed frames. Read-rate pacing consumes sixty media seconds in real time;
// gaps in decoded progress expose stalls but may also include decoder CPU cost.
func sustainedDecode(ctx context.Context, url, logPath string, started time.Time, firstFrame func() error) (map[string]any, error) {
	log, err := os.OpenFile(logPath, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0600)
	if err != nil {
		return nil, err
	}
	defer log.Close()
	cmd := exec.CommandContext(ctx, "ffmpeg", "-hide_banner", "-loglevel", "error", "-stats_period", "0.25", "-readrate", "1", "-i", url, "-t", "60", "-an", "-progress", "pipe:1", "-f", "null", "-")
	cmd.Stderr = log
	pipe, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err = cmd.Start(); err != nil {
		return nil, err
	}
	result := map[string]any{"kind": "paced_decoder_progress", "requested_media_seconds": 60}
	scanner := bufio.NewScanner(pipe)
	var first, lastAdvance time.Time
	var mediaUS, reportedUS, frame int64
	var maxGap time.Duration
	var gapsOverSecond int
	var frameError error
	for scanner.Scan() {
		key, value, ok := strings.Cut(scanner.Text(), "=")
		if !ok {
			continue
		}
		switch key {
		case "frame":
			frame, _ = strconv.ParseInt(strings.TrimSpace(value), 10, 64)
		case "out_time_us":
			reportedUS, _ = strconv.ParseInt(value, 10, 64)
		case "progress":
			now := time.Now()
			if frame > 0 && first.IsZero() {
				first = now
				lastAdvance = now
				result["first_decoded_progress_total_ms"] = float64(now.Sub(started).Microseconds()) / 1000
				frameError = firstFrame()
				if frameError != nil {
					_ = cmd.Process.Kill()
				}
			}
			if !first.IsZero() && reportedUS > mediaUS {
				gap := now.Sub(lastAdvance)
				maxGap = max(maxGap, gap)
				if gap > time.Second {
					gapsOverSecond++
				}
				lastAdvance, mediaUS = now, reportedUS
			}
		}
	}
	waitErr := cmd.Wait()
	result["elapsed_total_ms"] = float64(time.Since(started).Microseconds()) / 1000
	result["decoded_media_seconds"] = float64(mediaUS) / 1e6
	result["decoded_frames"] = frame
	result["max_progress_gap_ms"] = float64(maxGap.Microseconds()) / 1000
	result["progress_gaps_over_one_second"] = gapsOverSecond
	if !first.IsZero() {
		result["wall_after_first_progress_ms"] = float64(time.Since(first).Microseconds()) / 1000
	}
	if err := errors.Join(waitErr, scanner.Err(), frameError); err != nil {
		return result, err
	}
	if first.IsZero() || mediaUS < 59*1e6 {
		return result, errors.New("insufficient decoded duration")
	}
	return result, nil
}
