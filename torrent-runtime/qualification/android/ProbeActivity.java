// SPDX-License-Identifier: GPL-2.0-only
package org.playbackgateway.runtime.qualification;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.os.Process;
import java.io.File;

/** ABI qualification only. The UI process never loads the Go library. */
public final class ProbeActivity extends Activity {
    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        new File(getFilesDir(), "qualification.json").delete();
        startService(new Intent(this, ProbeService.class).putExtra("parent_pid", Process.myPid()));
    }
}
