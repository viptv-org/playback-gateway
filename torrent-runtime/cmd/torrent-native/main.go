// SPDX-License-Identifier: GPL-2.0-only
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"github.com/viptv-org/playback-gateway/torrent-runtime"
	"runtime/debug"
	"unsafe"
)

var controller torrentruntime.Controller

func init() { debug.SetMemoryLimit(128 << 20) }

//export VTRCall
func VTRCall(request *C.char, length C.int) *C.char {
	if request == nil || length < 0 || length > torrentruntime.MaxControlBytes {
		return C.CString(`{"version":2,"ok":false,"error":"invalid_control"}`)
	}
	return C.CString(string(controller.Call(C.GoBytes(unsafe.Pointer(request), length))))
}

//export VTRFree
func VTRFree(response *C.char) { C.free(unsafe.Pointer(response)) }

func main() {}
