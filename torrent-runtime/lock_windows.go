// SPDX-License-Identifier: GPL-2.0-only
package torrentruntime

import (
	"golang.org/x/sys/windows"
	"os"
	"path/filepath"
	"unsafe"
)

func lockCache(f *os.File) error {
	return windows.LockFileEx(windows.Handle(f.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK|windows.LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0, &windows.Overlapped{})
}

func diskGeometry(root string) (int64, int64, error) {
	full, err := filepath.Abs(root)
	if err != nil {
		return 0, 0, err
	}
	volume, err := windows.UTF16PtrFromString(filepath.VolumeName(full) + `\`)
	if err != nil {
		return 0, 0, err
	}
	var sectors, bytesPerSector, freeClusters, totalClusters uint32
	proc := windows.NewLazySystemDLL("kernel32.dll").NewProc("GetDiskFreeSpaceW")
	ok, _, err := proc.Call(uintptr(unsafe.Pointer(volume)), uintptr(unsafe.Pointer(&sectors)), uintptr(unsafe.Pointer(&bytesPerSector)), uintptr(unsafe.Pointer(&freeClusters)), uintptr(unsafe.Pointer(&totalClusters)))
	if ok == 0 {
		return 0, 0, err
	}
	var free, total, totalFree uint64
	err = windows.GetDiskFreeSpaceEx(volume, &free, &total, &totalFree)
	return int64(sectors) * int64(bytesPerSector), int64(free), err
}
