// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import "golang.org/x/sys/unix"

// Android and Linux authority must expire through device suspension.
func elapsedNow() (int64, error) {
	var value unix.Timespec
	err := unix.ClockGettime(unix.CLOCK_BOOTTIME, &value)
	return value.Nano(), err
}
