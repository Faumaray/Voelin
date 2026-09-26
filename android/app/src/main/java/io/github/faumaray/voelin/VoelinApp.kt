package io.github.faumaray.voelin

import android.app.Application

/** The process's application object; the Rust side reaches Android through it. */
class VoelinApp : Application() {
    override fun onCreate() {
        super.onCreate()
        instance = this
        Notifications.createChannels(this)
    }

    companion object {
        lateinit var instance: VoelinApp
            private set
    }
}
