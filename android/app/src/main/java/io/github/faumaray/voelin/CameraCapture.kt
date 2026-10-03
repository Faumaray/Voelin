package io.github.faumaray.voelin

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.ImageFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.OutputConfiguration
import android.hardware.camera2.params.SessionConfiguration
import android.hardware.display.DisplayManager
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Range
import android.util.Size
import android.view.Display
import android.view.Surface
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executor
import kotlin.math.abs

/**
 * Cameras for the Stream Studio (crates/voelin-android/src/camera.rs):
 * Camera2 into an ImageReader (YUV_420_888); each frame's planes go to
 * Native.onCameraFrame as direct buffers, valid during the call, so the
 * Rust side converts straight from the camera's memory. Several cameras
 * may run at once, each under the id the Rust side gave it.
 */
object CameraCapture {
    private const val TAG = "CameraCapture"

    /** Without a wish: about this size (16:9, what the studio composes). */
    private const val DEFAULT_WIDTH = 1280
    private const val DEFAULT_HEIGHT = 720

    private class Running(val id: Long, val cameraId: String) {
        @Volatile
        var stopped = false
        var thread: HandlerThread? = null
        var device: CameraDevice? = null
        var session: CameraCaptureSession? = null
        var reader: ImageReader? = null
    }

    private val running = ConcurrentHashMap<Long, Running>()

    /** Starts waiting for the CAMERA permission, by id. */
    private val waiting = ConcurrentHashMap<Long, () -> Unit>()

    private fun facing(c: CameraCharacteristics): String = when (c.get(CameraCharacteristics.LENS_FACING)) {
        CameraCharacteristics.LENS_FACING_FRONT -> "front"
        CameraCharacteristics.LENS_FACING_BACK -> "back"
        else -> "external"
    }

    private fun sizes(c: CameraCharacteristics): List<Size> =
        c.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            ?.getOutputSizes(ImageFormat.YUV_420_888)
            ?.sortedByDescending { it.width.toLong() * it.height }
            .orEmpty()

    private fun fpsRanges(c: CameraCharacteristics): List<Range<Int>> =
        c.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)?.toList().orEmpty()

    /**
     * The cameras, front ones first: "id<TAB>facing<TAB>sizes<TAB>maxFps"
     * lines, sizes as "1920x1080,1280x720" (largest first).
     */
    fun list(context: Context): String {
        val manager = context.getSystemService(CameraManager::class.java) ?: return ""
        val ids = try {
            manager.cameraIdList.toList()
        } catch (e: Exception) {
            Log.w(TAG, "cannot list the cameras", e)
            return ""
        }
        return ids.mapNotNull { id ->
            try {
                val c = manager.getCameraCharacteristics(id)
                val sizes = sizes(c).joinToString(",") { "${it.width}x${it.height}" }
                val fps = fpsRanges(c).maxOfOrNull { it.upper } ?: 30
                Triple(id, facing(c), "$sizes\t$fps")
            } catch (e: Exception) {
                Log.w(TAG, "cannot ask camera $id", e)
                null
            }
        }
            .sortedBy { (_, facing, _) -> if (facing == "front") 0 else if (facing == "back") 1 else 2 }
            .joinToString("\n") { (id, facing, rest) -> "$id\t$facing\t$rest" }
    }

    /**
     * Open `cameraId` for mixer feed `id` at about `width` x `height` (0:
     * the default) and `fps`. Asks for the CAMERA permission first if
     * needed; failures later go to Native.onCameraError.
     */
    fun start(context: Context, id: Long, cameraId: String, width: Int, height: Int, fps: Int): Boolean {
        if (context.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            return open(context, id, cameraId, width, height, fps)
        }
        val activity = MainActivity.current?.get()
        if (activity == null) {
            Native.onCameraError(id, "Allow Voelin to use the camera (open the app)")
            return false
        }
        waiting[id] = { open(context, id, cameraId, width, height, fps) }
        activity.runOnUiThread { activity.requestCameraPermission() }
        return true
    }

    /** The answer to the CAMERA permission prompt (MainActivity). */
    fun onPermissionResult(granted: Boolean) {
        val starts = waiting.keys.toList().mapNotNull { id -> waiting.remove(id)?.let { id to it } }
        for ((id, start) in starts) {
            if (granted) {
                start()
            } else {
                Native.onCameraError(id, "Voelin may not use the camera (Android settings, Apps, Voelin, Permissions)")
            }
        }
    }

    private fun open(context: Context, id: Long, cameraId: String, width: Int, height: Int, fps: Int): Boolean {
        stop(id)
        val manager = context.getSystemService(CameraManager::class.java) ?: return false
        val c = try {
            manager.getCameraCharacteristics(cameraId)
        } catch (e: Exception) {
            Native.onCameraError(id, "no camera $cameraId: ${e.message}")
            return false
        }
        val size = pick(sizes(c), width, height) ?: run {
            Native.onCameraError(id, "camera $cameraId has no YUV output")
            return false
        }
        val range = fpsRange(fpsRanges(c), fps)
        val front = c.get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_FRONT
        val sensor = c.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0
        val display = context.getSystemService(DisplayManager::class.java)?.getDisplay(Display.DEFAULT_DISPLAY)

        val run = Running(id, cameraId)
        running[id] = run
        val thread = HandlerThread("voelin-camera-$id").also { it.start() }
        run.thread = thread
        val handler = Handler(thread.looper)
        val executor = Executor { handler.post(it) }
        val reader = ImageReader.newInstance(size.width, size.height, ImageFormat.YUV_420_888, 3)
        run.reader = reader
        reader.setOnImageAvailableListener({ r ->
            val image = try {
                r.acquireLatestImage()
            } catch (e: IllegalStateException) {
                null
            } ?: return@setOnImageAvailableListener
            try {
                if (run.stopped) return@setOnImageAvailableListener
                val planes = image.planes
                val wanted = Native.onCameraFrame(
                    id,
                    planes[0].buffer,
                    planes[1].buffer,
                    planes[2].buffer,
                    planes[0].rowStride,
                    planes[1].rowStride,
                    planes[1].pixelStride,
                    image.width,
                    image.height,
                    rotation(sensor, front, display),
                    image.timestamp,
                )
                if (!wanted) stop(id)
            } finally {
                image.close()
            }
        }, handler)

        val fail = { message: String ->
            if (!run.stopped) Native.onCameraError(id, message)
            stop(id)
        }
        try {
            manager.openCamera(cameraId, executor, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    if (run.stopped) {
                        device.close()
                        return
                    }
                    run.device = device
                    val request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                        addTarget(reader.surface)
                        if (range != null) set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, range)
                    }
                    val outputs = listOf(OutputConfiguration(reader.surface))
                    device.createCaptureSession(
                        SessionConfiguration(
                            SessionConfiguration.SESSION_REGULAR,
                            outputs,
                            executor,
                            object : CameraCaptureSession.StateCallback() {
                                override fun onConfigured(session: CameraCaptureSession) {
                                    if (run.stopped) {
                                        session.close()
                                        return
                                    }
                                    run.session = session
                                    try {
                                        session.setRepeatingRequest(request.build(), null, handler)
                                    } catch (e: Exception) {
                                        fail("camera $cameraId: ${e.message}")
                                    }
                                }

                                override fun onConfigureFailed(session: CameraCaptureSession) {
                                    fail("camera $cameraId cannot deliver ${size.width}x${size.height}")
                                }
                            },
                        ),
                    )
                }

                override fun onDisconnected(device: CameraDevice) {
                    device.close()
                    // Another app took it, or it was unplugged.
                    fail("camera $cameraId was disconnected")
                }

                override fun onError(device: CameraDevice, error: Int) {
                    device.close()
                    fail("camera $cameraId failed (error $error)")
                }
            })
        } catch (e: SecurityException) {
            fail("Voelin may not use the camera")
            return false
        } catch (e: Exception) {
            fail("cannot open camera $cameraId: ${e.message}")
            return false
        }
        return true
    }

    fun stop(id: Long) {
        waiting.remove(id)
        val run = running.remove(id) ?: return
        run.stopped = true
        try {
            run.session?.close()
            run.device?.close()
        } catch (e: Exception) {
            Log.w(TAG, "closing camera ${run.cameraId}", e)
        }
        run.thread?.let { thread ->
            // The reader is closed on its own thread, after the last frame.
            Handler(thread.looper).post { run.reader?.close() }
            thread.quitSafely()
        }
    }

    /** The wished size if the camera has it, else the nearest in pixels (16:9 first). */
    private fun pick(sizes: List<Size>, width: Int, height: Int): Size? {
        if (sizes.isEmpty()) return null
        val (w, h) = if (width > 0 && height > 0) width to height else DEFAULT_WIDTH to DEFAULT_HEIGHT
        sizes.firstOrNull { it.width == w && it.height == h }?.let { return it }
        val wanted = w.toLong() * h
        val sameShape = sizes.filter { it.width.toLong() * h == it.height.toLong() * w }
        return (sameShape.ifEmpty { sizes }).minByOrNull { abs(it.width.toLong() * it.height - wanted) }
    }

    /** A fixed `fps` range if there is one, else the nearest that reaches it. */
    private fun fpsRange(ranges: List<Range<Int>>, fps: Int): Range<Int>? =
        ranges.firstOrNull { it.lower == fps && it.upper == fps }
            ?: ranges.filter { it.upper >= fps }.minWithOrNull(compareBy({ it.upper }, { -it.lower }))
            ?: ranges.maxByOrNull { it.upper }

    /**
     * Clockwise degrees that turn the sensor's picture upright for the
     * screen's current rotation (front cameras turn the other way).
     */
    private fun rotation(sensor: Int, front: Boolean, display: Display?): Int {
        val screen = when (display?.rotation) {
            Surface.ROTATION_90 -> 90
            Surface.ROTATION_180 -> 180
            Surface.ROTATION_270 -> 270
            else -> 0
        }
        return if (front) (sensor + screen) % 360 else (sensor - screen + 360) % 360
    }
}
