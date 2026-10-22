// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import "golang.org/x/sys/unix"

// Darwin MONOTONIC_RAW includes sleep, unlike mach_absolute_time. See Apple's
// mach_continuous_time documentation; leases must expire through suspension.
func elapsedNow() (int64, error) {
	var value unix.Timespec
	err := unix.ClockGettime(unix.CLOCK_MONOTONIC_RAW, &value)
	return value.Nano(), err
}
