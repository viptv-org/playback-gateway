// SPDX-License-Identifier: GPL-2.0-only
//go:build !linux && !windows

package torrentruntime

import "errors"

func elapsedNow() (int64, error) { return 0, errors.New("suspension-aware clock unavailable") }
