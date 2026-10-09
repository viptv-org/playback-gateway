// SPDX-License-Identifier: GPL-2.0-only
package org.playbackgateway.runtime;

import java.nio.charset.StandardCharsets;

/** Private worker-process bridge; never load this in the UI process. */
public final class NativeRuntime {
    static {
        System.loadLibrary("torrent_runtime_jni");
    }

    private NativeRuntime() {}

    private static native byte[] callNative(byte[] request);

    /**
     * The owner sends only authenticated private descriptors. A response with
     * terminate=true requires terminating this worker process; it must never
     * be reused after failed settlement. Responses are not analytics/log data.
     */
    public static String call(String request) {
        byte[] response = callNative(request.getBytes(StandardCharsets.UTF_8));
        if (response == null) {
            throw new IllegalStateException("Native transport response unavailable");
        }
        return new String(response, StandardCharsets.UTF_8);
    }
}
