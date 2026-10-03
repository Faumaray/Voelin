//! JNI glue with the app's Kotlin side (`android/app/src/main/java/...`).
//!
//! - Rust → Kotlin: static methods of `io.github.faumaray.voelin.Bridge`, called
//!   from any thread (the class is looked up once on the `android_main`
//!   thread, whose class loader sees the app's classes).
//! - Kotlin → Rust: the `external` methods of `io.github.faumaray.voelin.Native`,
//!   exported below with `native_method!` (panics and errors become Java
//!   exceptions instead of crossing the FFI boundary).

use std::sync::OnceLock;

use jni::errors::{Error, Result};
use jni::objects::{JByteBuffer, JClass, JFloatArray, JObject, JString, JValue};
use jni::refs::{Global, Reference as _};
use jni::sys::{jboolean, jint, jlong};
use jni::{Env, JavaVM, jni_sig, jni_str, native_method};

use crate::foreground::{ScreenNotice, VoiceNotice};

static BRIDGE: OnceLock<Global<JClass<'static>>> = OnceLock::new();

/// Look up `Bridge` (on the `android_main` thread).
pub fn init(env: &mut Env) -> Result<()> {
	if BRIDGE.get().is_none() {
		let class = env.load_class(jni_str!("io.github.faumaray.voelin.Bridge"))?;
		let _ = BRIDGE.set(env.new_global_ref(class)?);
	}
	Ok(())
}

/// Run `f` with an attached thread and the `Bridge` class.
fn with_bridge<T>(f: impl FnOnce(&mut Env, &Global<JClass<'static>>) -> Result<T>) -> Result<T> {
	let class = BRIDGE
		.get()
		.ok_or_else(|| Error::ClassNotFound { name: "io.github.faumaray.voelin.Bridge".into() })?;
	JavaVM::singleton()?.attach_current_thread(|env| f(env, class))
}

/// The application `Context`, as a local reference of `env`'s frame.
pub fn context<'local>(env: &mut Env<'local>) -> Result<JObject<'local>> {
	let class = BRIDGE
		.get()
		.ok_or_else(|| Error::ClassNotFound { name: "io.github.faumaray.voelin.Bridge".into() })?;
	env.call_static_method(
		class,
		jni_str!("context"),
		jni_sig!(() -> android.content.Context),
		&[],
	)?
	.l()
}

/// Ask for the microphone and notification permissions if not granted yet.
pub fn request_permissions() -> Result<()> {
	with_bridge(|env, class| {
		env.call_static_method(class, jni_str!("requestPermissions"), jni_sig!(() -> void), &[])?;
		Ok(())
	})
}

/// A Java string, or `null` for `None`.
fn string_or_null<'local>(env: &mut Env<'local>, text: Option<&str>) -> Result<JObject<'local>> {
	Ok(match text {
		Some(text) => JObject::from(env.new_string(text)?),
		None => JObject::null(),
	})
}

/// Show or update the voice service's notification; `None` stops the service.
pub fn set_voice_notification(notice: Option<&VoiceNotice>) -> Result<()> {
	with_bridge(|env, class| {
		let title = string_or_null(env, notice.map(|n| n.title.as_str()))?;
		let text = string_or_null(env, notice.map(|n| n.text.as_str()))?;
		let (muted, deafened, since) =
			notice.map_or((false, false, 0), |n| (n.muted, n.deafened, n.since_ms));
		env.call_static_method(
			class,
			jni_str!("setVoiceNotification"),
			jni_sig!(
				(
					title: java.lang.String,
					text: java.lang.String,
					muted: jboolean,
					deafened: jboolean,
					since_ms: jlong,
				) -> void
			),
			&[
				JValue::Object(&title),
				JValue::Object(&text),
				JValue::Bool(muted),
				JValue::Bool(deafened),
				JValue::Long(since as jlong),
			],
		)?;
		Ok(())
	})
}

/// What the screen-sharing notification says while our stream is live;
/// `None` puts back the plain "sharing your screen".
pub fn set_screen_notification(notice: Option<&ScreenNotice>) -> Result<()> {
	with_bridge(|env, class| {
		let title = string_or_null(env, notice.map(|n| n.title.as_str()))?;
		let text = string_or_null(env, notice.map(|n| n.text.as_str()))?;
		env.call_static_method(
			class,
			jni_str!("setScreenNotification"),
			jni_sig!((title: java.lang.String, text: java.lang.String) -> void),
			&[JValue::Object(&title), JValue::Object(&text)],
		)?;
		Ok(())
	})
}

/// Ask the user to share the screen. The answer comes back through
/// `Native.onScreenCaptureResult`.
pub fn request_screen_capture(fps: u32, max_size: u32) -> Result<()> {
	with_bridge(|env, class| {
		env.call_static_method(
			class,
			jni_str!("requestScreenCapture"),
			jni_sig!((fps: jint, max_size: jint) -> void),
			&[JValue::Int(fps as jint), JValue::Int(max_size as jint)],
		)?;
		Ok(())
	})
}

pub fn stop_screen_capture() -> Result<()> {
	with_bridge(|env, class| {
		env.call_static_method(class, jni_str!("stopScreenCapture"), jni_sig!(() -> void), &[])?;
		Ok(())
	})
}

/// Capture what the device plays; `false` without a running screen capture.
pub fn start_system_audio() -> Result<bool> {
	with_bridge(|env, class| {
		env.call_static_method(class, jni_str!("startSystemAudio"), jni_sig!(() -> jboolean), &[])?
			.z()
	})
}

pub fn stop_system_audio() -> Result<()> {
	with_bridge(|env, class| {
		env.call_static_method(class, jni_str!("stopSystemAudio"), jni_sig!(() -> void), &[])?;
		Ok(())
	})
}

/// Capture playback into mixer input `id` (delivered to
/// `Native.onAudioInput`): of the app `package`, or everything but ours.
/// `false` without a running screen capture, or for a package that is not
/// installed (or not visible to us).
pub fn start_audio_input(id: u64, package: Option<&str>) -> Result<bool> {
	with_bridge(|env, class| {
		let package = match package {
			Some(package) => JObject::from(env.new_string(package)?),
			None => JObject::null(),
		};
		env.call_static_method(
			class,
			jni_str!("startAudioInput"),
			jni_sig!((id: jlong, package: java.lang.String) -> jboolean),
			&[JValue::Long(id as jlong), JValue::Object(&package)],
		)?
		.z()
	})
}

pub fn stop_audio_input(id: u64) -> Result<()> {
	with_bridge(|env, class| {
		env.call_static_method(
			class,
			jni_str!("stopAudioInput"),
			jni_sig!((id: jlong) -> void),
			&[JValue::Long(id as jlong)],
		)?;
		Ok(())
	})
}

/// Launchable apps other than ours, as `(label, package)`, sorted by label.
pub fn launchable_apps() -> Result<Vec<(String, String)>> {
	let lines = with_bridge(|env, class| {
		let value = env
			.call_static_method(
				class,
				jni_str!("launchableApps"),
				jni_sig!(() -> java.lang.String),
				&[],
			)?
			.l()?;
		if value.is_null() {
			return Ok(String::new());
		}
		let value = env.cast_local::<JString>(value)?;
		value.try_to_string(env)
	})?;
	Ok(parse_apps(&lines))
}

/// `label<TAB>package` lines.
fn parse_apps(lines: &str) -> Vec<(String, String)> {
	lines
		.lines()
		.filter_map(|line| {
			let (label, package) = line.split_once('\t')?;
			let package = package.trim();
			let label = if label.trim().is_empty() { package } else { label.trim() };
			(!package.is_empty()).then(|| (label.to_owned(), package.to_owned()))
		})
		.collect()
}

pub fn secret_get(key: &str) -> Result<Option<String>> {
	with_bridge(|env, class| {
		let key = JObject::from(env.new_string(key)?);
		let value = env
			.call_static_method(
				class,
				jni_str!("secretGet"),
				jni_sig!((key: java.lang.String) -> java.lang.String),
				&[JValue::Object(&key)],
			)?
			.l()?;
		if value.is_null() {
			return Ok(None);
		}
		let value = env.cast_local::<JString>(value)?;
		Ok(Some(value.try_to_string(env)?))
	})
}

pub fn secret_set(key: &str, value: &str) -> Result<()> {
	with_bridge(|env, class| {
		let key = JObject::from(env.new_string(key)?);
		let value = JObject::from(env.new_string(value)?);
		env.call_static_method(
			class,
			jni_str!("secretSet"),
			jni_sig!((key: java.lang.String, value: java.lang.String) -> void),
			&[JValue::Object(&key), JValue::Object(&value)],
		)?;
		Ok(())
	})
}

pub fn secret_delete(key: &str) -> Result<()> {
	with_bridge(|env, class| {
		let key = JObject::from(env.new_string(key)?);
		env.call_static_method(
			class,
			jni_str!("secretDelete"),
			jni_sig!((key: java.lang.String) -> void),
			&[JValue::Object(&key)],
		)?;
		Ok(())
	})
}

// Native methods of `Native` (Kotlin `object` with `@JvmStatic external`).

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_screen_capture_result(granted: jboolean, error: JString),
};

fn on_screen_capture_result<'local>(
	env: &mut Env<'local>,
	_class: JClass<'local>,
	granted: jboolean,
	error: JString<'local>,
) -> Result<()> {
	let error = if error.is_null() { None } else { Some(error.try_to_string(env)?) };
	crate::capture::on_result(granted, error);
	Ok(())
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_screen_frame(
		pixels: JByteBuffer,
		width: jint,
		height: jint,
		row_stride: jint,
		timestamp_ns: jlong,
	) -> jboolean,
};

/// An RGBA frame from the `ImageReader`. Returns `false` when frames are no
/// longer wanted (the service then stops capturing).
fn on_screen_frame<'local>(
	env: &mut Env<'local>,
	_class: JClass<'local>,
	pixels: JByteBuffer<'local>,
	width: jint,
	height: jint,
	row_stride: jint,
	timestamp_ns: jlong,
) -> Result<jboolean> {
	if !crate::capture::wants_frames() {
		return Ok(false);
	}
	let address = env.get_direct_buffer_address(&pixels)?;
	let capacity = env.get_direct_buffer_capacity(&pixels)?;
	if address.is_null() {
		return Err(Error::NullPtr("screen frame buffer"));
	}
	// SAFETY: a direct buffer's memory is `capacity` bytes at `address`, and
	// the `Image` it belongs to stays open until this call returns.
	#[allow(unsafe_code)]
	let bytes = unsafe { std::slice::from_raw_parts(address, capacity) }.to_vec();
	Ok(crate::capture::on_frame(
		bytes,
		width.max(0) as u32,
		height.max(0) as u32,
		row_stride.max(0) as usize,
		timestamp_ns,
	))
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_screen_capture_stopped(),
};

fn on_screen_capture_stopped<'local>(_env: &mut Env<'local>, _class: JClass<'local>) -> Result<()> {
	crate::capture::on_stopped();
	Ok(())
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_system_audio(samples: jfloat[], count: jint, channels: jint, timestamp_ns: jlong) -> jboolean,
};

/// Interleaved 48 kHz float samples of what the device plays.
fn on_system_audio<'local>(
	env: &mut Env<'local>,
	_class: JClass<'local>,
	samples: JFloatArray<'local>,
	count: jint,
	channels: jint,
	timestamp_ns: jlong,
) -> Result<jboolean> {
	if !crate::capture::wants_audio() {
		return Ok(false);
	}
	let mut buffer = vec![0f32; count.max(0) as usize];
	samples.get_region(env, 0, &mut buffer)?;
	Ok(crate::capture::on_audio(buffer, channels.clamp(1, 8) as u16, timestamp_ns))
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_audio_input(id: jlong, samples: jfloat[], count: jint, channels: jint) -> jboolean,
};

/// Interleaved 48 kHz float samples for mixer input `id` (see
/// [`start_audio_input`]). Returns `false` once the input is gone.
fn on_audio_input<'local>(
	env: &mut Env<'local>,
	_class: JClass<'local>,
	id: jlong,
	samples: JFloatArray<'local>,
	count: jint,
	channels: jint,
) -> Result<jboolean> {
	thread_local! {
		// One per capture thread, reused for every call. (It is `const`;
		// clippy for Android does not see that through the expansion.)
		#[allow(clippy::missing_const_for_thread_local)]
		static BUFFER: std::cell::RefCell<Vec<f32>> = const { std::cell::RefCell::new(Vec::new()) };
	}
	let id = id as u64;
	if !crate::capture::wants_input(id) {
		return Ok(false);
	}
	BUFFER.with_borrow_mut(|buffer| {
		buffer.resize(count.max(0) as usize, 0.0);
		samples.get_region(env, 0, buffer)?;
		Ok(crate::capture::on_input(id, buffer, channels.clamp(1, 8) as u16))
	})
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_disconnect_voice(),
};

/// "Disconnect" in the voice notification.
fn on_disconnect_voice<'local>(_env: &mut Env<'local>, _class: JClass<'local>) -> Result<()> {
	crate::app::disconnect_voice();
	Ok(())
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_toggle_mute(),
};

/// "Mute" / "Unmute" in the voice notification.
fn on_toggle_mute<'local>(_env: &mut Env<'local>, _class: JClass<'local>) -> Result<()> {
	crate::app::toggle_mute();
	Ok(())
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_toggle_deafen(),
};

/// "Deafen" / "Undeafen" in the voice notification.
fn on_toggle_deafen<'local>(_env: &mut Env<'local>, _class: JClass<'local>) -> Result<()> {
	crate::app::toggle_deafen();
	Ok(())
}

const _: jni::NativeMethod = native_method! {
	java_type = "io.github.faumaray.voelin.Native",
	static extern fn on_open_voice(),
};

/// The voice notification was tapped: show the voice channel.
fn on_open_voice<'local>(_env: &mut Env<'local>, _class: JClass<'local>) -> Result<()> {
	voelin_ui::request(voelin_ui::Request::ShowVoice);
	Ok(())
}
