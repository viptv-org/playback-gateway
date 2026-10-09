// SPDX-License-Identifier: GPL-2.0-only
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"github.com/viptv-org/playback-gateway/torrent-runtime"
	"os"
	"runtime/debug"
)

func main() {
	debug.SetMemoryLimit(128 << 20)
	controller := new(torrentruntime.Controller)
	fmt.Println(`{"version":2,"ready":true,"qualification":"candidate"}`)
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), torrentruntime.MaxControlBytes)
	for scanner.Scan() {
		raw := controller.Call(scanner.Bytes())
		fmt.Println(string(raw))
		var response struct {
			Terminate bool `json:"terminate"`
		}
		_ = json.Unmarshal(raw, &response)
		if response.Terminate {
			os.Exit(3)
		}
	}
	response := controller.Call([]byte(`{"version":2,"op":"shutdown"}`))
	var result struct {
		Terminate bool `json:"terminate"`
	}
	_ = json.Unmarshal(response, &result)
	if result.Terminate || scanner.Err() != nil {
		os.Exit(3)
	}
}
