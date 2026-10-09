// SPDX-License-Identifier: GPL-2.0-only
package org.playbackgateway.runtime.qualification;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.os.Process;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import org.json.JSONObject;
import org.playbackgateway.runtime.NativeRuntime;

/** Loads the packaged JNI bridge and exercises control in an app-private process. */
public final class ProbeService extends Service {
    @Override public IBinder onBind(Intent intent) { return null; }

    @Override public int onStartCommand(Intent intent, int flags, int startId) {
        int parent = intent == null ? -1 : intent.getIntExtra("parent_pid", -1);
        new Thread(() -> runProbe(parent), "runtime-abi-probe").start();
        return START_NOT_STICKY;
    }

    private static JSONObject call(JSONObject command) throws Exception {
        JSONObject result = new JSONObject(NativeRuntime.call(command.toString()));
        if (!result.optBoolean("ok")) throw new IllegalStateException("control refused");
        return result;
    }

    private void runProbe(int parent) {
        JSONObject result = new JSONObject();
        boolean opened = false;
        try {
            if (parent <= 0 || parent == Process.myPid()) throw new IllegalStateException("worker isolation missing");
            JSONObject config = new JSONObject().put("cache_dir", new File(getCacheDir(), "pieces").getPath())
                .put("cache_bytes", 16 * 1024 * 1024);
            call(new JSONObject().put("version", 2).put("op", "open").put("config", config));
            opened = true;
            JSONObject rejected = new JSONObject(NativeRuntime.call(new JSONObject().put("version", 2)
                .put("op", "prepare").put("source", new JSONObject().put("magnet", "invalid"))
                .put("authority_ms", 60000).toString()));
            if (rejected.optBoolean("ok")) throw new IllegalStateException("invalid source admitted");
            call(new JSONObject().put("version", 2).put("op", "shutdown"));
            opened = false;
            result.put("passed", true).put("worker_isolated", true)
                .put("sdk", android.os.Build.VERSION.SDK_INT).put("abi", android.os.Build.SUPPORTED_ABIS[0]);
        } catch (Throwable failure) {
            // This fixture opens no private source. Linker details stay in its
            // app-private qualification artifact, never product telemetry.
            try { result.put("passed", false).put("error_class", failure.getClass().getSimpleName())
                .put("detail", failure.toString()); }
            catch (Exception ignored) { }
        } finally {
            if (opened) {
                try { NativeRuntime.call("{\"version\":2,\"op\":\"shutdown\"}"); } catch (Throwable ignored) { }
            }
            try (FileOutputStream out = new FileOutputStream(new File(getFilesDir(), "qualification.json"))) {
                out.write(result.toString().getBytes(StandardCharsets.UTF_8));
            } catch (Exception ignored) { }
            stopSelf();
        }
    }
}
