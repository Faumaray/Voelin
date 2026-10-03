package io.github.faumaray.voelin

import android.app.Application
import java.io.File

/** The process's application object; the Rust side reaches Android through it. */
class VoelinApp : Application() {
    override fun onCreate() {
        super.onCreate()
        instance = this
        Notifications.createChannels(this)
        // Copies of files shared to us (MainActivity.share) from before.
        File(cacheDir, "shared").deleteRecursively()
    }

    companion object {
        lateinit var instance: VoelinApp
            private set
    }
}
