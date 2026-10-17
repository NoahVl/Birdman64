// Birdman64's one Java class: the activity, plus the SAF ROM picker result
// forwarding. The framework's android.app.NativeActivity cannot pass
// onActivityResult to native code (no such callback in ANativeActivity), so
// this subclass forwards the picked document URI into libbirdman64.so
// (crates/birdman64/src/android/rom_picker.rs). Compiled by build-apk.sh.
package app.birdman64;

import android.app.NativeActivity;
import android.content.Intent;

public class PwActivity extends NativeActivity {
    static {
        // The game library must be loaded through the JVM for this class's
        // native methods to resolve: NativeActivity's own load of it is a
        // raw dlopen (not registered with the JVM), so without this the
        // picker callback throws UnsatisfiedLinkError when it runs. The
        // OS refcounts dlopen: android_main is not called again.
        System.loadLibrary("birdman64");
    }

    /** The ROM pick request code, started by the native side
     *  (rom_picker.rs ROM_PICK_REQUEST). */
    static final int ROM_PICK_REQUEST = 4001;

    /** Save export (create a document) and import (open one) requests
     *  (android/save_picker.rs). */
    static final int SAVE_EXPORT_REQUEST = 4002;
    static final int SAVE_IMPORT_REQUEST = 4003;

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != ROM_PICK_REQUEST
                && requestCode != SAVE_EXPORT_REQUEST
                && requestCode != SAVE_IMPORT_REQUEST) {
            return;
        }
        String uri = resultCode == RESULT_OK && data != null && data.getData() != null
                ? data.getData().toString()
                : "";
        if (requestCode == ROM_PICK_REQUEST) {
            nativeRomPicked(uri);
        } else {
            nativeSavePicked(requestCode, uri);
        }
    }

    /** Implemented in libbirdman64.so (save_picker.rs). "" = cancelled. */
    static native void nativeSavePicked(int request, String uri);

    /** Implemented in libbirdman64.so (rom_picker.rs). "" = cancelled. The
     *  static block above has loaded the library, so this resolves. */
    static native void nativeRomPicked(String uri);
}
