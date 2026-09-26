package io.github.faumaray.tsc

import android.app.NativeActivity
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import java.lang.ref.WeakReference

/**
 * The window: a NativeActivity running `android_main` of libtsc_android.so
 * (the Slint UI). Kotlin only handles what needs an Activity: permission
 * prompts and the screen-capture consent dialog.
 */
class MainActivity : NativeActivity() {
    private var captureFps = 30
    private var captureMaxSize = 1920

    override fun onCreate(savedInstanceState: Bundle?) {
        current = WeakReference(this)
        super.onCreate(savedInstanceState)
    }

    override fun onDestroy() {
        if (current?.get() === this) current = null
        super.onDestroy()
    }

    /**
     * Back leaves the app like Home instead of finishing the activity, so the
     * window (and its state) stays while voice goes on in the background.
     */
    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        moveTaskToBack(true)
    }

    /** Show the system's "start recording or casting?" dialog. */
    fun requestScreenCapture(fps: Int, maxSize: Int) {
        captureFps = fps
        captureMaxSize = maxSize
        val manager = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        startActivityForResult(manager.createScreenCaptureIntent(), REQUEST_SCREEN_CAPTURE)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_SCREEN_CAPTURE) return
        if (resultCode == RESULT_OK && data != null) {
            // The service reports the outcome to Native.onScreenCaptureResult.
            ScreenCaptureService.start(this, resultCode, data, captureFps, captureMaxSize)
        } else {
            Native.onScreenCaptureResult(false, null)
        }
    }

    companion object {
        private const val REQUEST_SCREEN_CAPTURE = 1001

        /** The activity in front, if any. */
        @Volatile
        var current: WeakReference<MainActivity>? = null
            private set
    }
}
