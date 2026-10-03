package io.github.faumaray.voelin

import java.nio.ByteBuffer

/** Callbacks into the Rust library (crates/voelin-android/src/bridge.rs). */
object Native {
    init {
        // Already loaded by the NativeActivity; this registers it for JNI
        // lookups from this class loader.
        System.loadLibrary("voelin_android")
    }

    /** The user answered the screen-capture dialog (`error`: why it failed). */
    @JvmStatic
    external fun onScreenCaptureResult(granted: Boolean, error: String?)

    /** An RGBA_8888 frame. Returns false when frames are no longer wanted. */
    @JvmStatic
    external fun onScreenFrame(
        pixels: ByteBuffer,
        width: Int,
        height: Int,
        rowStride: Int,
        timestampNs: Long,
    ): Boolean

    /** The projection ended (by the app, the user or the system). */
    @JvmStatic
    external fun onScreenCaptureStopped()

    /**
     * `count` interleaved 48 kHz float samples of what the device plays.
     * Returns false when audio is no longer wanted.
     */
    @JvmStatic
    external fun onSystemAudio(samples: FloatArray, count: Int, channels: Int, timestampNs: Long): Boolean

    /**
     * `count` interleaved 48 kHz float samples for mixer input `id`
     * (Bridge.startAudioInput). Returns false when it is no longer wanted.
     */
    @JvmStatic
    external fun onAudioInput(id: Long, samples: FloatArray, count: Int, channels: Int): Boolean

    /** "Disconnect" in the voice notification. */
    @JvmStatic
    external fun onDisconnectVoice()

    /** "Mute" / "Unmute" in the voice notification. */
    @JvmStatic
    external fun onToggleMute()

    /** "Deafen" / "Undeafen" in the voice notification. */
    @JvmStatic
    external fun onToggleDeafen()

    /**
     * A YUV_420_888 camera frame for feed `id`: the three planes (direct
     * buffers, valid during the call), the luma and chroma row strides, the
     * chroma pixel stride (1: planar, 2: interleaved), and the clockwise
     * rotation that turns it upright. Returns false when it is no longer
     * wanted.
     */
    @JvmStatic
    external fun onCameraFrame(
        id: Long,
        y: ByteBuffer,
        u: ByteBuffer,
        v: ByteBuffer,
        yRowStride: Int,
        uvRowStride: Int,
        uvPixelStride: Int,
        width: Int,
        height: Int,
        rotation: Int,
        timestampNs: Long,
    ): Boolean

    /** Camera feed `id` failed or could not start (`message`: why). */
    @JvmStatic
    external fun onCameraError(id: Long, message: String)

    /** The voice notification was tapped: show the voice channel. */
    @JvmStatic
    external fun onOpenVoice()

    /**
     * Shared to the app ("Share to Voelin"): text for the current chat's
     * composer, and copies of the shared files (one path per line) for the
     * current channel.
     */
    @JvmStatic
    external fun onShare(text: String?, paths: String?)
}
