// SPDX-License-Identifier: GPL-2.0-only
//go:build !windows

package torrentruntime

import (
	"os"
	"syscall"
)

func lockCache(f *os.File) error { return syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB) }

func diskGeometry(root string) (int64, int64, error) {
	var st syscall.Statfs_t
	err := syscall.Statfs(root, &st)
	return int64(st.Bsize), int64(st.Bavail) * int64(st.Bsize), err
}
