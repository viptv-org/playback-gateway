// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import "golang.org/x/sys/windows"

func elapsedNow() (int64, error) { return int64(windows.DurationSinceBoot()), nil }
