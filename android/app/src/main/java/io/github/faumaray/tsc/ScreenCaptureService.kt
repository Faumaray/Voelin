package io.github.faumaray.tsc

import android.Manifest
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.Looper
import android.util.Log
import android.view.Display
import android.view.Surface
import kotlin.concurrent.thread
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * Screen sharing: holds the MediaProjection (a `mediaProjection` foreground
 * service, as Android requires), mirrors the display into an ImageReader and
 * hands each RGBA frame to Native.onScreenFrame; optionally captures what the
 * device plays (AudioPlaybackCapture) for Native.onSystemAudio.
 */
class ScreenCaptureService : Service() {
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var worker: HandlerThread? = null

    @Volatile
    private var audioRunning = false
    private var audioThread: Thread? = null

    private val main = Handler(Looper.getMainLooper())

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent == null || intent.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (projection != null) return START_NOT_STICKY
        val stop = PendingIntent.getService(
            this,
            0,
            Intent(this, ScreenCaptureService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notifications.ongoing(
            this,
            Notifications.CHANNEL_SCREEN,
            getString(R.string.screen_sharing),
            getString(R.string.stop_sharing) to stop,
        )
        try {
            // Must be in the foreground before getMediaProjection (Android 14).
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
            val data = if (Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(EXTRA_DATA, Intent::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(EXTRA_DATA)
            } ?: throw IllegalStateException("no projection data")
            val manager = getSystemService(MediaProjectionManager::class.java)
            val projection = manager.getMediaProjection(intent.getIntExtra(EXTRA_RESULT, 0), data)
                ?: throw IllegalStateException("the projection was refused")
            this.projection = projection
            instance = this
            startVideo(projection, intent.getIntExtra(EXTRA_FPS, 30), intent.getIntExtra(EXTRA_MAX_SIZE, 1920))
            Native.onScreenCaptureResult(true, null)
        } catch (e: Exception) {
            Log.w(TAG, "screen capture failed", e)
            Native.onScreenCaptureResult(false, e.message ?: e.toString())
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun startVideo(projection: MediaProjection, fps: Int, maxSize: Int) {
        val worker = HandlerThread("tsc-screen").also { it.start() }
        this.worker = worker
        val handler = Handler(worker.looper)
        // The user or the system can end the projection (status bar chip,
        // screen lock); registered before the virtual display (Android 14).
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                main.post { stopSelf() }
            }
        }, handler)

        val metrics = resources.displayMetrics
        val screen = getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
        val mode = screen.mode
        // The mode is in the display's natural orientation; match the current
        // one. (Rotating while sharing letterboxes the picture.)
        val rotated = screen.rotation == Surface.ROTATION_90 || screen.rotation == Surface.ROTATION_270
        val (fullWidth, fullHeight) = if (rotated) {
            mode.physicalHeight to mode.physicalWidth
        } else {
            mode.physicalWidth to mode.physicalHeight
        }
        val scale = minOf(1.0, maxSize.toDouble() / max(fullWidth, fullHeight))
        // Even sizes, as video encoders want.
        val width = ((fullWidth * scale).roundToInt() / 2 * 2).coerceAtLeast(2)
        val height = ((fullHeight * scale).roundToInt() / 2 * 2).coerceAtLeast(2)

        val reader = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 3)
        this.reader = reader
        val interval = 1_000_000_000L / fps.coerceIn(1, 60)
        var last = 0L
        reader.setOnImageAvailableListener({ r ->
            val image = r.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                // The display renders at its own rate; pass on at most `fps`.
                if (image.timestamp - last < interval) return@setOnImageAvailableListener
                last = image.timestamp
                val plane = image.planes[0]
                val wanted = Native.onScreenFrame(plane.buffer, image.width, image.height, plane.rowStride, image.timestamp)
                if (!wanted) main.post { stopSelf() }
            } finally {
                image.close()
            }
        }, handler)
        display = projection.createVirtualDisplay(
            "tsc-screen",
            width,
            height,
            metrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            reader.surface,
            null,
            handler,
        )
    }

    /** Capture media, game and unknown-usage playback as 48 kHz stereo float. */
    private fun startAudioCapture(): Boolean {
        val projection = projection ?: return false
        if (audioRunning) return true
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            return false
        }
        val config = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        val minimum = AudioRecord.getMinBufferSize(SAMPLE_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_FLOAT)
        val record = try {
            AudioRecord.Builder()
                .setAudioFormat(format)
                .setAudioPlaybackCaptureConfig(config)
                // A tenth of a second.
                .setBufferSizeInBytes(max(minimum, SAMPLE_RATE / 10 * 2 * 4))
                .build()
        } catch (e: Exception) {
            Log.w(TAG, "system audio capture unavailable", e)
            return false
        }
        audioRunning = true
        record.startRecording()
        audioThread = thread(name = "tsc-system-audio") {
            // 10 ms of stereo.
            val buffer = FloatArray(SAMPLE_RATE / 100 * 2)
            try {
                while (audioRunning) {
                    val read = record.read(buffer, 0, buffer.size, AudioRecord.READ_BLOCKING)
                    if (read < 0) break
                    if (read > 0 && !Native.onSystemAudio(buffer, read, 2, System.nanoTime())) break
                }
            } finally {
                audioRunning = false
                record.stop()
                record.release()
            }
        }
        return true
    }

    private fun stopAudioCapture() {
        audioRunning = false
        audioThread?.join(500)
        audioThread = null
    }

    override fun onDestroy() {
        if (instance === this) instance = null
        stopAudioCapture()
        display?.release()
        display = null
        reader?.close()
        reader = null
        projection?.stop()
        projection = null
        worker?.quitSafely()
        worker = null
        Native.onScreenCaptureStopped()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "ScreenCaptureService"
        private const val ID = 2
        private const val SAMPLE_RATE = 48_000
        private const val ACTION_STOP = "io.github.faumaray.tsc.STOP_SHARING"
        private const val EXTRA_RESULT = "result"
        private const val EXTRA_DATA = "data"
        private const val EXTRA_FPS = "fps"
        private const val EXTRA_MAX_SIZE = "max_size"

        @Volatile
        private var instance: ScreenCaptureService? = null

        /** Start capturing with the consent the user just gave. */
        fun start(context: Context, resultCode: Int, data: Intent, fps: Int, maxSize: Int) {
            val intent = Intent(context, ScreenCaptureService::class.java)
                .putExtra(EXTRA_RESULT, resultCode)
                .putExtra(EXTRA_DATA, data)
                .putExtra(EXTRA_FPS, fps)
                .putExtra(EXTRA_MAX_SIZE, maxSize)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, ScreenCaptureService::class.java))
        }

        fun startAudio(): Boolean {
            val service = instance ?: return false
            return service.startAudioCapture()
        }

        fun stopAudio() {
            instance?.stopAudioCapture()
        }
    }
}
