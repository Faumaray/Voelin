package io.github.faumaray.voelin

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
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
import android.os.Process
import android.util.Log
import android.view.Display
import android.view.Surface
import java.util.concurrent.ConcurrentHashMap
import kotlin.concurrent.thread
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * Screen sharing: holds the MediaProjection (a `mediaProjection` foreground
 * service, as Android requires), mirrors the display into an ImageReader and
 * hands each RGBA frame to Native.onScreenFrame; optionally captures what
 * other apps play (AudioPlaybackCapture: all but us, or one app) for
 * Native.onSystemAudio and Native.onAudioInput.
 */
class ScreenCaptureService : Service() {
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var worker: HandlerThread? = null

    /** A running AudioPlaybackCapture recording. */
    private class AudioCapture {
        @Volatile
        var running = true

        @Volatile
        var thread: Thread? = null
    }

    /** Recordings by id: SYSTEM_AUDIO, or a mixer input of the Rust side. */
    private val captures = ConcurrentHashMap<Long, AudioCapture>()

    private val main = Handler(Looper.getMainLooper())

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent == null || intent.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (projection != null) return START_NOT_STICKY
        try {
            // Must be in the foreground before getMediaProjection (Android 14).
            startForeground(ID, notification(this), ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
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
        val worker = HandlerThread("voelin-screen").also { it.start() }
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
            "voelin-screen",
            width,
            height,
            metrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            reader.surface,
            null,
            handler,
        )
    }

    /**
     * Capture media, game and unknown-usage playback as 48 kHz stereo float:
     * of app `uid`, or with null of every app but us (our voices and watched
     * streams). Several recordings may run at once.
     */
    private fun startAudioCapture(id: Long, uid: Int?): Boolean {
        val projection = projection ?: return false
        if (captures[id]?.running == true) return true
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            return false
        }
        val builder = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
        // Uid rules cannot mix matching and excluding; one of them.
        if (uid != null) builder.addMatchingUid(uid) else builder.excludeUid(Process.myUid())
        val config = builder.build()
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
            Log.w(TAG, "playback capture unavailable", e)
            return false
        }
        val capture = AudioCapture()
        captures[id] = capture
        record.startRecording()
        capture.thread = thread(name = "voelin-audio-$id") {
            // 10 ms of stereo.
            val buffer = FloatArray(SAMPLE_RATE / 100 * 2)
            try {
                while (capture.running) {
                    val read = record.read(buffer, 0, buffer.size, AudioRecord.READ_BLOCKING)
                    if (read < 0) break
                    if (read == 0) continue
                    val wanted = if (id == SYSTEM_AUDIO) {
                        Native.onSystemAudio(buffer, read, 2, System.nanoTime())
                    } else {
                        Native.onAudioInput(id, buffer, read, 2)
                    }
                    if (!wanted) break
                }
            } finally {
                capture.running = false
                captures.remove(id, capture)
                record.stop()
                record.release()
            }
        }
        return true
    }

    private fun stopAudioCapture(id: Long) {
        val capture = captures.remove(id) ?: return
        capture.running = false
        capture.thread?.join(500)
    }

    override fun onDestroy() {
        if (instance === this) instance = null
        captures.keys.toList().forEach { stopAudioCapture(it) }
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

        /** The recording for Native.onSystemAudio (Rust input ids start at 1). */
        private const val SYSTEM_AUDIO = 0L
        private const val ACTION_STOP = "io.github.faumaray.voelin.STOP_SHARING"
        private const val EXTRA_RESULT = "result"
        private const val EXTRA_DATA = "data"
        private const val EXTRA_FPS = "fps"
        private const val EXTRA_MAX_SIZE = "max_size"

        @Volatile
        private var instance: ScreenCaptureService? = null

        /** Where our stream is live and who watches ("Live in Chill Zone"), or null. */
        @Volatile
        private var notice: Pair<String, String>? = null

        private fun notification(context: Context): Notification {
            val stop = PendingIntent.getService(
                context,
                0,
                Intent(context, ScreenCaptureService::class.java).setAction(ACTION_STOP),
                PendingIntent.FLAG_IMMUTABLE,
            )
            val (title, text) = notice ?: (context.getString(R.string.screen_sharing) to context.getString(R.string.screen_sharing_text))
            return Notifications.ongoing(
                context,
                Notifications.CHANNEL_SCREEN,
                title,
                text,
                Notifications.openApp(context),
                0,
                context.getString(R.string.stop_sharing) to stop,
            )
        }

        /** What the notification says while our stream is live (null: the default). */
        fun setNotice(title: String?, text: String?) {
            notice = if (title != null) title to (text ?: "") else null
            val service = instance ?: return
            service.getSystemService(NotificationManager::class.java).notify(ID, notification(service))
        }

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
            return service.startAudioCapture(SYSTEM_AUDIO, null)
        }

        fun stopAudio() {
            instance?.stopAudioCapture(SYSTEM_AUDIO)
        }

        /** Capture `packageName` (null: every app but us) into mixer input `id`. */
        fun startInput(context: Context, id: Long, packageName: String?): Boolean {
            val service = instance ?: return false
            val uid = if (packageName == null) {
                null
            } else {
                try {
                    val pm = context.packageManager
                    if (Build.VERSION.SDK_INT >= 33) {
                        pm.getApplicationInfo(packageName, PackageManager.ApplicationInfoFlags.of(0)).uid
                    } else {
                        @Suppress("DEPRECATION")
                        pm.getApplicationInfo(packageName, 0).uid
                    }
                } catch (e: PackageManager.NameNotFoundException) {
                    Log.w(TAG, "no package $packageName to capture")
                    return false
                }
            }
            return service.startAudioCapture(id, uid)
        }

        fun stopInput(id: Long) {
            instance?.stopAudioCapture(id)
        }
    }
}
