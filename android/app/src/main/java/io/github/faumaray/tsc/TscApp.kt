package io.github.faumaray.tsc

import android.app.Application

/** The process's application object; the Rust side reaches Android through it. */
class TscApp : Application() {
    override fun onCreate() {
        super.onCreate()
        instance = this
        Notifications.createChannels(this)
    }

    companion object {
        lateinit var instance: TscApp
            private set
    }
}
