//! Video encoders through FFmpeg: the backends, their realtime settings, the
//! encoder session and the startup self-test.
//!
//! Every backend is configured for live streaming: no B-frames, no
//! lookahead, keyframes only at the start and on request (an "infinite"
//! GOP, forced IDR), constant bitrate. The bitrate changes on the running
//! encoder where FFmpeg's wrapper applies it (x264, NVENC, Quick Sync);
//! elsewhere at the next keyframe, or at once when it drops below half
//! (congestion). Frame and packet buffers are allocated once per session
//! and reused; VA-API surfaces come from a pool.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::gpu::{self, GpuConverter, GpuLayer, RGB_FOURCCS};
use super::layout::{self, HwFramesFields};
use super::sys::{self, FrameHead, PacketHead, Ptr, Rational, cstr};
use super::{Ffmpeg, take_log, vpp};
use crate::capture::{DmaBufRef, drm_fourcc};
use crate::codec::hw::EncoderFactory;
use crate::codec::{
	Codec, ContentHint, EncodedChunk, EncodedFrame, EncoderBackend, EncoderConfig, H264Profile,
	VideoEncoder,
};
use crate::convert;
use crate::frame::{FrameData, GpuFrame, VideoFrame};
use crate::{Error, Result};

/// How a backend takes frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
	/// Planar YUV 4:2:0 in system memory.
	Yuv420p,
	/// NV12 in system memory.
	Nv12,
	/// NV12 uploaded into a VA-API surface pool (or DMA-BUFs mapped to
	/// surfaces).
	Vaapi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
	/// A GPU or OS encoder.
	Hardware,
	/// A software encoder that FFmpeg wraps (x264, SVT-AV1, ...).
	Software,
}

/// One FFmpeg encoder this crate knows how to drive.
#[derive(Debug, PartialEq, Eq)]
pub struct BackendSpec {
	/// FFmpeg's encoder name, also the name in settings
	/// (`stream.encoder_backend`).
	pub name: &'static str,
	pub codec: Codec,
	/// The API or library behind it, for the UI.
	pub api: &'static str,
	pub kind: BackendKind,
	pub input: Input,
	/// FFmpeg applies bitrate changes to the running encoder.
	pub dynamic_bitrate: bool,
	/// Keyframe interval that means "only on request" for this wrapper.
	pub infinite_gop: i64,
}

const INFINITE: i64 = 1 << 30;

macro_rules! backend {
	($name:literal, $codec:ident, $api:literal, $kind:ident, $input:ident, $dynamic:literal, $gop:expr) => {
		BackendSpec {
			name: $name,
			codec: Codec::$codec,
			api: $api,
			kind: BackendKind::$kind,
			input: Input::$input,
			dynamic_bitrate: $dynamic,
			infinite_gop: $gop,
		}
	};
}

/// Every backend, in order of preference within its codec and kind. The
/// ones FFmpeg or the platform lacks fail the probe and are skipped.
pub static BACKENDS: &[BackendSpec] = &[
	// H.264
	backend!("h264_nvenc", H264, "NVENC", Hardware, Yuv420p, true, INFINITE),
	backend!("h264_amf", H264, "AMF", Hardware, Nv12, false, 0),
	backend!("h264_vaapi", H264, "VA-API", Hardware, Vaapi, false, INFINITE),
	backend!("h264_qsv", H264, "Quick Sync", Hardware, Nv12, true, 65535),
	backend!("h264_videotoolbox", H264, "VideoToolbox", Hardware, Nv12, false, 0),
	backend!("h264_mf", H264, "Media Foundation", Hardware, Nv12, false, INFINITE),
	backend!("libx264", H264, "x264", Software, Yuv420p, true, INFINITE),
	backend!("libopenh264", H264, "OpenH264", Software, Yuv420p, false, INFINITE),
	// HEVC (offered last: only used when a viewer takes nothing else)
	backend!("hevc_nvenc", H265, "NVENC", Hardware, Yuv420p, true, INFINITE),
	backend!("hevc_amf", H265, "AMF", Hardware, Nv12, false, 0),
	backend!("hevc_vaapi", H265, "VA-API", Hardware, Vaapi, false, INFINITE),
	backend!("hevc_qsv", H265, "Quick Sync", Hardware, Nv12, true, 65535),
	backend!("hevc_videotoolbox", H265, "VideoToolbox", Hardware, Nv12, false, 0),
	backend!("hevc_mf", H265, "Media Foundation", Hardware, Nv12, false, INFINITE),
	// AV1
	backend!("av1_nvenc", Av1, "NVENC", Hardware, Yuv420p, true, INFINITE),
	backend!("av1_amf", Av1, "AMF", Hardware, Nv12, false, 0),
	backend!("av1_vaapi", Av1, "VA-API", Hardware, Vaapi, false, INFINITE),
	backend!("av1_qsv", Av1, "Quick Sync", Hardware, Nv12, true, 65535),
	backend!("libsvtav1", Av1, "SVT-AV1", Software, Yuv420p, false, INFINITE),
	// rav1e refuses a keyframe interval of 2^30.
	backend!("librav1e", Av1, "rav1e", Software, Yuv420p, false, 1 << 29),
	backend!("libaom-av1", Av1, "libaom", Software, Yuv420p, false, INFINITE),
	// VP9 and VP8 (software VP8 / VP9 is libvpx, used directly)
	backend!("vp9_vaapi", Vp9, "VA-API", Hardware, Vaapi, false, INFINITE),
	backend!("vp9_qsv", Vp9, "Quick Sync", Hardware, Nv12, true, 65535),
	backend!("vp8_vaapi", Vp8, "VA-API", Hardware, Vaapi, false, INFINITE),
];

impl BackendSpec {
	pub fn by_name(name: &str) -> Option<&'static BackendSpec> {
		BACKENDS.iter().find(|b| b.name == name)
	}

	pub fn is_hardware(&self) -> bool {
		self.kind == BackendKind::Hardware
	}

	/// Whether the automatic choice may use it: rav1e holds about 20 frames
	/// before its first packet even in its low-latency mode (measured with
	/// rav1e 0.7), so it is only used when named in the settings.
	pub fn is_automatic(&self) -> bool {
		self.name != "librav1e"
	}

	fn is_vaapi(&self) -> bool {
		self.input == Input::Vaapi
	}

	fn family(&self) -> &'static str {
		self.name.rsplit('_').next().unwrap_or(self.name)
	}
}

/// Where an option is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
	/// `AVCodecContext` (generic options).
	Codec,
	/// The encoder's private options (`priv_data`).
	Private,
}

/// One option with alternative values (tried in order: older releases
/// know other names), optional unless `required`.
struct Setting {
	target: Target,
	name: &'static str,
	values: Vec<String>,
	required: bool,
}

fn opt(name: &'static str, values: &[&str]) -> Setting {
	Setting {
		target: Target::Private,
		name,
		values: values.iter().map(|v| (*v).to_owned()).collect(),
		required: false,
	}
}

fn generic(name: &'static str, value: impl ToString) -> Setting {
	Setting { target: Target::Codec, name, values: vec![value.to_string()], required: false }
}

/// libx264 presets, fastest first (`EncoderConfig::speed` indexes them).
const X264_PRESETS: [&str; 10] = [
	"ultrafast",
	"superfast",
	"veryfast",
	"faster",
	"fast",
	"medium",
	"slow",
	"slower",
	"veryslow",
	"placebo",
];

/// `FF_PROFILE_H264_HIGH` and `FF_PROFILE_H264_CONSTRAINED_BASELINE`.
const PROFILE_H264_HIGH: i64 = 100;
const PROFILE_H264_CONSTRAINED_BASELINE: i64 = 66 | 1 << 9;

/// H.264 level limits (ITU-T H.264 Table A-1): `level_idc`, macroblocks per
/// second, macroblocks per frame, bitrate in kbit/s for Baseline (High
/// allows 1.25 times that).
const H264_LEVELS: [(i64, u64, u64, u64); 19] = [
	(10, 1_485, 99, 64),
	(11, 3_000, 396, 192),
	(12, 6_000, 396, 384),
	(13, 11_880, 396, 768),
	(20, 11_880, 396, 2_000),
	(21, 19_800, 792, 4_000),
	(22, 20_250, 1_620, 4_000),
	(30, 40_500, 1_620, 10_000),
	(31, 108_000, 3_600, 14_000),
	(32, 216_000, 5_120, 20_000),
	(40, 245_760, 8_192, 20_000),
	(41, 245_760, 8_192, 50_000),
	(42, 522_240, 8_704, 50_000),
	(50, 589_824, 22_080, 135_000),
	(51, 983_040, 36_864, 240_000),
	(52, 2_073_600, 36_864, 240_000),
	(60, 4_177_920, 139_264, 240_000),
	(61, 8_355_840, 139_264, 480_000),
	(62, 16_711_680, 139_264, 800_000),
];

/// The lowest `level_idc` whose limits hold this stream, or the highest
/// level defined if none does.
///
/// The signalling offers the same level to the viewer
/// (`voelin_stream::h264::level_idc`, which this mirrors — a test pins the
/// two together). It has to be told to the encoder, because wrappers that
/// pick a level themselves pick a generous one: AMF writes level 4.2 into
/// the SPS of a 720p30 stream whose offer says 3.1, and a decoder set up
/// from that offer may refuse it.
fn h264_level(profile: H264Profile, width: u32, height: u32, fps: u32, bitrate: u32) -> i64 {
	let (mbs_w, mbs_h) = (u64::from(width.div_ceil(16)), u64::from(height.div_ceil(16)));
	let frame = mbs_w * mbs_h;
	let rate = frame * u64::from(fps.max(1));
	let factor = match profile {
		H264Profile::ConstrainedHigh => 1250,
		H264Profile::ConstrainedBaseline => 1000,
	};
	H264_LEVELS
		.iter()
		.find(|&&(_, max_rate, max_frame, max_kbps)| {
			rate <= max_rate
				&& frame <= max_frame
				// Neither side may exceed sqrt(8 * MaxFS) macroblocks.
				&& mbs_w * mbs_w <= 8 * max_frame
				&& mbs_h * mbs_h <= 8 * max_frame
				&& u64::from(bitrate) <= max_kbps * factor
		})
		.map_or(H264_LEVELS[H264_LEVELS.len() - 1].0, |l| l.0)
}

/// The backend's realtime settings (besides size, format, time base and
/// rate, which every backend gets).
fn settings(spec: &BackendSpec, config: &EncoderConfig, low_power: bool) -> Vec<Setting> {
	let screen = config.content == ContentHint::Screen;
	let baseline = config.h264_profile == H264Profile::ConstrainedBaseline;
	let h264 = spec.codec == Codec::H264;
	let generic_profile =
		if baseline { PROFILE_H264_CONSTRAINED_BASELINE } else { PROFILE_H264_HIGH };
	let mut list = Vec::new();
	match spec.family() {
		"nvenc" => {
			let preset = config.speed.map(|s| format!("p{}", s.clamp(1, 7)));
			let mut presets = preset.into_iter().collect::<Vec<_>>();
			presets.extend(["p2".into(), "llhp".into()]);
			list.push(Setting { values: presets, ..opt("preset", &[]) });
			list.push(opt("tune", &["ull", "ll"]));
			list.push(opt("rc", &["cbr"]));
			list.push(opt("zerolatency", &["1"]));
			list.push(opt("delay", &["0"]));
			list.push(opt("rc-lookahead", &["0"]));
			list.push(opt("forced-idr", &["1"]));
			list.push(opt("no-scenecut", &["1"]));
			list.push(opt("b_ref_mode", &["disabled"]));
			if h264 {
				list.push(opt("profile", if baseline { &["baseline"] } else { &["high"] }));
			}
		}
		"qsv" => {
			list.push(opt("preset", &["veryfast"]));
			list.push(opt("async_depth", &["1"]));
			list.push(opt("look_ahead", &["0"]));
			list.push(opt("look_ahead_depth", &["0"]));
			list.push(opt("forced_idr", &["1"]));
			list.push(opt("low_delay_brc", &["1"]));
			list.push(opt(
				"scenario",
				if screen { &["displayremoting"] } else { &["videoconference"] },
			));
			if h264 {
				list.push(opt("profile", if baseline { &["baseline"] } else { &["high"] }));
			}
		}
		"amf" => {
			list.push(opt("usage", &["ultralowlatency", "lowlatency"]));
			list.push(opt("quality", &["speed"]));
			list.push(opt("rc", &["cbr"]));
			list.push(opt("header_insertion_mode", &["idr", "key", "gop"]));
			list.push(opt("latency", &["1"]));
			if h264 {
				list.push(opt(
					"profile",
					if baseline {
						&["constrained_baseline", "main"]
					} else {
						&["constrained_high", "high"]
					},
				));
			}
		}
		"vaapi" => {
			list.push(opt("rc_mode", &["CBR"]));
			list.push(opt("async_depth", &["1"]));
			if low_power {
				list.push(Setting { required: true, ..opt("low_power", &["1"]) });
			}
			if h264 {
				list.push(generic("profile", generic_profile));
			}
		}
		"videotoolbox" => {
			list.push(opt("realtime", &["1"]));
			list.push(opt("prio_speed", &["1"]));
			list.push(opt("allow_sw", &["0"]));
			if h264 {
				list.push(opt(
					"profile",
					if baseline {
						&["constrained_baseline", "baseline"]
					} else {
						&["constrained_high", "high"]
					},
				));
			}
		}
		"mf" => {
			list.push(opt("hw_encoding", &["1"]));
			list.push(opt("rate_control", &["cbr"]));
			list.push(opt(
				"scenario",
				if screen { &["display_remoting"] } else { &["video_conference"] },
			));
			if h264 {
				list.push(generic("profile", generic_profile));
			}
		}
		_ => match spec.name {
			"libx264" => {
				let preset = config.speed.map_or("veryfast", |s| {
					X264_PRESETS[s.clamp(0, X264_PRESETS.len() as i32 - 1) as usize]
				});
				list.push(Setting { required: true, ..opt("preset", &[preset]) });
				list.push(opt("tune", &["zerolatency"]));
				list.push(opt("profile", if baseline { &["baseline"] } else { &["high"] }));
				list.push(opt("forced-idr", &["1"]));
				list.push(opt("x264-params", &["scenecut=0"]));
			}
			"libopenh264" => {
				list.push(generic("profile", generic_profile));
				list.push(opt("allow_skip_frames", &["1"]));
				list.push(opt("rc_mode", &["bitrate"]));
			}
			"libsvtav1" => {
				let preset = config.speed.unwrap_or(10).to_string();
				list.push(Setting { values: vec![preset], ..opt("preset", &[]) });
				list.push(opt(
					"svtav1-params",
					&["rc=2:pred-struct=1:lookahead=0:scd=0", "rc=2:pred-struct=1"],
				));
			}
			"librav1e" => {
				let speed = config.speed.unwrap_or(10).to_string();
				list.push(Setting { values: vec![speed], ..opt("speed", &[]) });
				list.push(opt(
					"rav1e-params",
					&["low_latency=true:rdo_lookahead_frames=1:reservoir_frame_delay=12"],
				));
			}
			"libaom-av1" => {
				let cpu_used = config.speed.unwrap_or(8).to_string();
				list.push(opt("usage", &["realtime"]));
				list.push(Setting { values: vec![cpu_used], ..opt("cpu-used", &[]) });
				list.push(opt("lag-in-frames", &["0"]));
				list.push(opt("row-mt", &["1"]));
				list.push(opt("end-usage", &["cbr"]));
				list.push(opt("aq-mode", &["3"]));
				list.push(opt("enable-tpl-model", &["0"]));
				if screen {
					list.push(opt("tune-content", &["screen"]));
				}
			}
			_ => {}
		},
	}
	list
}

/// The process's VA-API device: `VOELIN_VAAPI_DEVICE`, or the first DRM
/// render node.
struct Device(Ptr);

// SAFETY: an AVBufferRef of an AVHWDeviceContext, never freed; FFmpeg's
// VA-API device may be used from several threads (libva locks per display).
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// The DRM render node for VA-API.
pub fn vaapi_device_path() -> Option<String> {
	if let Some(path) = std::env::var_os("VOELIN_VAAPI_DEVICE") {
		return Some(path.to_string_lossy().into_owned());
	}
	let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
		.ok()?
		.flatten()
		.map(|e| e.path().to_string_lossy().into_owned())
		.filter(|p| p.rsplit('/').next().is_some_and(|n| n.starts_with("renderD")))
		.collect();
	nodes.sort();
	nodes.into_iter().next()
}

fn vaapi_device(ffmpeg: &Ffmpeg) -> std::result::Result<Ptr, String> {
	static DEVICE: OnceLock<std::result::Result<Device, String>> = OnceLock::new();
	DEVICE
		.get_or_init(|| {
			let path = vaapi_device_path().ok_or("no DRM render node (/dev/dri/renderD*)")?;
			let api = &ffmpeg.api;
			let name = cstr("vaapi");
			// SAFETY: a C string.
			let kind = unsafe { (api.av_hwdevice_find_type_by_name)(name.as_ptr()) };
			if kind <= 0 {
				return Err("FFmpeg has no VA-API support".into());
			}
			let mut device = std::ptr::null_mut();
			let c_path = cstr(&path);
			// SAFETY: out pointer and C strings are valid; no options.
			let ret = unsafe {
				(api.av_hwdevice_ctx_create)(
					&mut device,
					kind,
					c_path.as_ptr(),
					std::ptr::null_mut(),
					0,
				)
			};
			if ret < 0 {
				return Err(format!(
					"VA-API device {path}: {}{}",
					api.error_text(ret),
					log_suffix("vaapi")
				));
			}
			Ok(Device(device))
		})
		.as_ref()
		.map(|d| d.0)
		.map_err(Clone::clone)
}

/// The `VADisplay` of the process's VA-API device (the first field of its
/// `AVVAAPIDeviceContext`).
pub(crate) fn vaapi_display(ffmpeg: &Ffmpeg) -> std::result::Result<vpp::Display, String> {
	let device = vaapi_device(ffmpeg)?;
	let name = cstr("vaapi");
	// SAFETY: a C string; `device` is a live VA-API device reference, whose
	// API context starts with the display.
	unsafe {
		let kind = (ffmpeg.api.av_hwdevice_find_type_by_name)(name.as_ptr());
		let hwctx = layout::device_hwctx(device, kind)?;
		Ok(layout::read::<Ptr>(hwctx, 0))
	}
}

/// A pool of VA-API surfaces of `sw_format` (an `AVPixelFormat`) and
/// `width` x `height` on the process's device (an `AVHWFramesContext`
/// reference); surfaces are made on demand and recycled.
pub(crate) fn vaapi_pool(
	ffmpeg: &Ffmpeg,
	width: u32,
	height: u32,
	sw_format: c_int,
) -> std::result::Result<Ptr, String> {
	let api = &ffmpeg.api;
	let device = vaapi_device(ffmpeg)?;
	let vaapi = ffmpeg.pix.vaapi.ok_or("no VA-API pixel format")?;
	// SAFETY: `device` is a live device reference.
	let mut pool = unsafe { (api.av_hwframe_ctx_alloc)(device) };
	if pool.is_null() {
		return Err("av_hwframe_ctx_alloc failed".into());
	}
	// SAFETY: `pool` was just allocated on `device`.
	let fields: HwFramesFields = match unsafe { layout::hw_frames_fields(pool, device) } {
		Ok(f) => f,
		Err(e) => {
			// SAFETY: allocated above.
			unsafe { (api.av_buffer_unref)(&mut pool) };
			return Err(e);
		}
	};
	// SAFETY: the fields were located and checked on this very context;
	// setting them before av_hwframe_ctx_init is what FFmpeg expects.
	let ret = unsafe {
		let ctx = (*pool.cast::<sys::BufferRefHead>()).data.cast::<std::ffi::c_void>();
		layout::write::<c_int>(ctx, fields.format, vaapi);
		layout::write::<c_int>(ctx, fields.sw_format, sw_format);
		layout::write::<c_int>(ctx, fields.width, width as c_int);
		layout::write::<c_int>(ctx, fields.height, height as c_int);
		// Surfaces are allocated on demand and recycled by the pool.
		layout::write::<c_int>(ctx, fields.initial_pool_size, 0);
		(api.av_hwframe_ctx_init)(pool)
	};
	if ret < 0 {
		// SAFETY: allocated above.
		unsafe { (api.av_buffer_unref)(&mut pool) };
		return Err(format!(
			"VA-API surface pool: {}{}",
			api.error_text(ret),
			log_suffix("AVHWFramesContext")
		));
	}
	Ok(pool)
}

/// FFmpeg's recent messages, appended to an error.
fn log_suffix(context: &str) -> String {
	let log = take_log(context);
	if log.is_empty() { String::new() } else { format!(" ({})", log.join("; ")) }
}

/// Maps output packets back to the frames' 90 kHz timestamps.
struct Timestamps {
	pending: VecDeque<(i64, u64)>,
}

impl Timestamps {
	fn new() -> Self {
		Self { pending: VecDeque::with_capacity(64) }
	}

	fn push(&mut self, pts: i64, pts_90khz: u64) {
		if self.pending.len() == self.pending.capacity() {
			self.pending.pop_front();
		}
		self.pending.push_back((pts, pts_90khz));
	}

	fn take(&mut self, pts: i64) -> Option<u64> {
		while let Some(&(p, t)) = self.pending.front() {
			if p > pts {
				break;
			}
			self.pending.pop_front();
			if p == pts {
				return Some(t);
			}
		}
		None
	}
}

/// An open encoder for one size and frame rate.
struct Session {
	ctx: Ptr,
	width: u32,
	height: u32,
	fps: u32,
	bitrate: u32,
	/// Frame in system memory, filled per frame (reused).
	sw: Ptr,
	/// VA-API: the surface pool and the frame that takes a surface.
	pool: Ptr,
	hw: Ptr,
	packet: Ptr,
	last_pts: Option<i64>,
	/// A frame went in (so there is something to flush).
	sent: bool,
	/// No packet came out yet: the first one starts the stream, so it is a
	/// keyframe even where the wrapper does not flag it (rav1e).
	first_packet: bool,
	/// When it was opened (bitrate increases reopen at most every
	/// [`REOPEN_FOR_BITRATE`]).
	opened: Instant,
	timestamps: Timestamps,
	nv12: bool,
	/// An AMF encoder: opened and closed under [`amf_lock`].
	amf: bool,
}

// SAFETY: the FFmpeg objects belong to this session alone and are used from
// one thread at a time (`&mut self`).
unsafe impl Send for Session {}

/// Held while an AMF encoder is opened or closed.
///
/// AMD's AMF runtime keeps process-wide state that is not safe to use from
/// two threads at once: its factory helper loads and unloads libraries
/// (`AMFFactoryHelper::Init` / `Terminate`, `dlclose`) while it creates the
/// Vulkan device of a new encoder. Opened on several threads together, as
/// the probe and simulcast layers do, one thread's `Terminate` unloads what
/// another's `Init` uses: SIGSEGV in `libamfrt64` inside `avcodec_open2`.
/// Measured with probes under Xvfb, several processes at once: 10 crashes
/// in 420 runs before, none in 360 with opens one at a time.
fn amf_lock() -> std::sync::MutexGuard<'static, ()> {
	static AMF: std::sync::Mutex<()> = std::sync::Mutex::new(());
	AMF.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Session {
	fn drop(&mut self) {
		let api = &Ffmpeg::get().expect("a session exists only with FFmpeg").api;
		// SAFETY: each pointer is NULL or owned by this session.
		unsafe {
			(api.av_frame_free)(&mut self.sw);
			(api.av_frame_free)(&mut self.hw);
			(api.av_packet_free)(&mut self.packet);
			let amf = self.amf.then(amf_lock);
			(api.avcodec_free_context)(&mut self.ctx);
			drop(amf);
			if !self.pool.is_null() {
				(api.av_buffer_unref)(&mut self.pool);
			}
		}
	}
}

/// How long a session runs before a bitrate increase of half or more
/// reopens it (a keyframe) on backends that cannot change the bitrate
/// running; smaller increases wait for the next keyframe.
const REOPEN_FOR_BITRATE: Duration = Duration::from_secs(5);

/// When to recreate the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reinit {
	No,
	/// With the next keyframe (bitrate changes the wrapper cannot apply).
	AtKeyframe,
	/// Before the next frame.
	Now,
}

/// An encoder of one [`BackendSpec`].
pub struct FfmpegEncoder {
	ffmpeg: &'static Ffmpeg,
	spec: &'static BackendSpec,
	config: EncoderConfig,
	session: Option<Session>,
	reinit: Reinit,
	/// VA-API only offers the low-power entry point for this codec.
	low_power: bool,
	/// `AVFrame.hw_frames_ctx` was checked on a surface of this encoder.
	dmabuf_checked: bool,
	/// Sizes it encodes exactly are multiples of these
	/// ([`BackendStatus::alignment`]).
	alignment: (u32, u32),
	/// For RGB DMA-BUFs ([`encode_dmabuf`](Self::encode_dmabuf)): made on
	/// the first one; why not, if it cannot be.
	gpu: Option<std::result::Result<GpuConverter, String>>,
}

/// `size` cropped to a multiple of `alignment` (even at least, for 4:2:0
/// chroma; at least 2x2): what an encoder of that alignment encodes exactly.
pub(crate) fn exact_size(size: (u32, u32), alignment: (u32, u32)) -> (u32, u32) {
	let crop = |v: u32, a: u32| {
		let a = a.max(2);
		if v >= a { v / a * a } else { (v & !1).max(2) }
	};
	(crop(size.0, alignment.0), crop(size.1, alignment.1))
}

impl FfmpegEncoder {
	/// An encoder of backend `name` (nothing is opened until the first
	/// frame, whose size it takes).
	pub fn new(name: &str, config: EncoderConfig) -> Result<Self> {
		let spec = BackendSpec::by_name(name).ok_or_else(|| Error::CodecUnavailable {
			codec: Codec::H264,
			reason: format!("unknown FFmpeg encoder {name}"),
		})?;
		if let Some(reason) = amf_refused().filter(|_| spec.family() == "amf") {
			return Err(Error::CodecUnavailable { codec: spec.codec, reason: reason.into() });
		}
		let ffmpeg = Ffmpeg::get()
			.map_err(|e| Error::CodecUnavailable { codec: spec.codec, reason: e.to_owned() })?;
		// What the probe measured, once it is done (its own encoders run
		// before that, at sizes every backend encodes exactly).
		let alignment = PROBE
			.get()
			.and_then(|p| p.iter().find(|s| s.spec == spec))
			.map_or((2, 2), |s| s.alignment);
		Ok(Self {
			ffmpeg,
			spec,
			config,
			session: None,
			reinit: Reinit::No,
			low_power: false,
			dmabuf_checked: false,
			alignment,
			gpu: None,
		})
	}

	pub fn spec(&self) -> &'static BackendSpec {
		self.spec
	}

	fn error(&self, what: &str, code: c_int) -> Error {
		Error::Encoder {
			codec: self.spec.codec,
			message: format!(
				"{} {what}: {}{}",
				self.spec.name,
				self.ffmpeg.api.error_text(code),
				log_suffix(self.spec.name)
			),
		}
	}

	fn unavailable(&self, reason: String) -> Error {
		Error::CodecUnavailable { codec: self.spec.codec, reason }
	}

	fn set_option(&self, obj: Ptr, name: &str, value: &str) -> c_int {
		let (name, value) = (cstr(name), cstr(value));
		// SAFETY: an AVClass-enabled object and two C strings.
		unsafe { (self.ffmpeg.api.av_opt_set)(obj, name.as_ptr(), value.as_ptr(), 0) }
	}

	fn set_int(&self, obj: Ptr, name: &str, value: i64) -> c_int {
		let name = cstr(name);
		// SAFETY: an AVClass-enabled object and a C string.
		unsafe { (self.ffmpeg.api.av_opt_set_int)(obj, name.as_ptr(), value, 0) }
	}

	/// Apply one setting; an optional one that this release does not know
	/// (or does not accept) is skipped.
	fn apply(&self, ctx: Ptr, private: Ptr, setting: &Setting) -> Result<()> {
		let obj = match setting.target {
			Target::Codec => ctx,
			Target::Private if private.is_null() => return Ok(()),
			Target::Private => private,
		};
		let mut last = sys::OPTION_NOT_FOUND;
		for value in &setting.values {
			last = self.set_option(obj, setting.name, value);
			if last >= 0 {
				return Ok(());
			}
		}
		if setting.required {
			return Err(self.error(&format!("option {}", setting.name), last));
		}
		if last != sys::OPTION_NOT_FOUND {
			tracing::debug!(
				backend = self.spec.name,
				option = setting.name,
				"not accepted: {}",
				self.ffmpeg.api.error_text(last)
			);
		}
		Ok(())
	}

	/// The encoder size: the frame cropped to a multiple of the backend's
	/// alignment, so that the stream declares exactly the picture it
	/// carries ([`exact_size`]).
	fn coded_size(&self, width: u32, height: u32) -> (u32, u32) {
		exact_size((width, height), self.alignment)
	}

	/// Open a session for `width` x `height`.
	fn open(&mut self, width: u32, height: u32) -> Result<Session> {
		match self.open_with(width, height, self.low_power) {
			Err(e) if self.spec.is_vaapi() && !self.low_power => {
				// Some GPUs only have the low-power entry point for a codec.
				match self.open_with(width, height, true) {
					Ok(session) => {
						self.low_power = true;
						Ok(session)
					}
					Err(_) => Err(e),
				}
			}
			result => result,
		}
	}

	fn open_with(&self, width: u32, height: u32, low_power: bool) -> Result<Session> {
		let api = &self.ffmpeg.api;
		let pix = self.ffmpeg.pix;
		let name = cstr(self.spec.name);
		// SAFETY: a C string; returns a static codec or NULL.
		let codec = unsafe { (api.avcodec_find_encoder_by_name)(name.as_ptr()) };
		if codec.is_null() {
			return Err(self.unavailable(format!("{} is not in this FFmpeg build", self.spec.name)));
		}
		// SAFETY: allocates a context with the codec's private options.
		let ctx = unsafe { (api.avcodec_alloc_context3)(codec) };
		if ctx.is_null() {
			return Err(self.unavailable("avcodec_alloc_context3 failed".into()));
		}
		let fps = self.config.fps.max(1);
		let bitrate = self.config.bitrate_bps.max(1);
		let mut session = Session {
			ctx,
			width,
			height,
			fps,
			bitrate,
			sw: std::ptr::null_mut(),
			pool: std::ptr::null_mut(),
			hw: std::ptr::null_mut(),
			packet: std::ptr::null_mut(),
			last_pts: None,
			sent: false,
			first_packet: true,
			opened: Instant::now(),
			timestamps: Timestamps::new(),
			nv12: self.spec.input != Input::Yuv420p,

			amf: false,
		};
		// SAFETY: `ctx` is a live codec context; the first child is its
		// private options object (NULL if the codec has none).
		let private = unsafe { (api.av_opt_child_next)(ctx, std::ptr::null_mut()) };
		let (w, h) = (width as c_int, height as c_int);
		let sw_format = if session.nv12 { pix.nv12 } else { pix.yuv420p };
		let format = match self.spec.input {
			Input::Vaapi => {
				pix.vaapi.ok_or_else(|| self.unavailable("no VA-API pixel format".into()))?
			}
			_ => sw_format,
		};
		let size = cstr("video_size");
		let pixel_format = cstr("pixel_format");
		let time_base = cstr("time_base");
		// SAFETY: typed option setters on a live context with C strings.
		let ret = unsafe {
			let mut ret = (api.av_opt_set_image_size)(ctx, size.as_ptr(), w, h, 0);
			if ret >= 0 {
				ret = (api.av_opt_set_pixel_fmt)(ctx, pixel_format.as_ptr(), format, 0);
			}
			if ret >= 0 {
				let tb = Rational { num: 1, den: fps as c_int };
				ret = (api.av_opt_set_q)(ctx, time_base.as_ptr(), tb, 0);
			}
			ret
		};
		if ret < 0 {
			return Err(self.error("size, format and time base", ret));
		}
		let gop = self.config.keyframe_interval.map_or(self.spec.infinite_gop, i64::from);
		let mut list = vec![
			generic("b", bitrate),
			// SVT-AV1 takes CBR from its own `rc=2` and rejects a maximum
			// that is not above the target.
			generic("maxrate", if self.spec.name == "libsvtav1" { 0 } else { bitrate }),
			// A one-second buffer, except for VA-API AV1: with one second
			// Mesa's rate control overshot by 3.5 % at 1080p60 / 6 Mbit/s and
			// 7.9 % at 1440p60 / 10 Mbit/s (RX 7900 GRE, desktop pattern);
			// with two it lands within 1.2 % at both and at 720p30, no frame
			// skipped, the largest 68 KB against 64 KB at 1440p60.
			generic(
				"bufsize",
				u64::from(bitrate) * if self.spec.name == "av1_vaapi" { 2 } else { 1 },
			),
			generic("g", gop),
			generic("bf", 0),
			generic("colorspace", "smpte170m"),
			generic("color_primaries", "smpte170m"),
			generic("color_trc", "smpte170m"),
			generic("color_range", "tv"),
		];
		if !self.spec.is_hardware() {
			list.push(generic("threads", self.config.threads_for(width, height)));
		}
		if self.spec.codec == Codec::H264 {
			// The level the signalling offers, so the SPS cannot claim a
			// higher one than the viewer's decoder was set up for. Both
			// targets: the wrappers that have a private `level` read that
			// one, the rest `AVCodecContext.level`.
			let level =
				h264_level(self.config.h264_profile, width, height, fps, bitrate).to_string();
			list.push(generic("level", &level));
			list.push(Setting { values: vec![level], ..opt("level", &[]) });
		}
		list.extend(settings(self.spec, &self.config, low_power));
		for setting in &list {
			self.apply(ctx, private, setting)?;
		}
		if self.spec.is_vaapi() {
			session.pool = vaapi_pool(self.ffmpeg, width, height, self.ffmpeg.pix.nv12)
				.map_err(|e| self.unavailable(e))?;
			let offset = self.ffmpeg.codec_hw_frames.clone().map_err(|e| self.unavailable(e))?;
			// SAFETY: `offset` is AVCodecContext.hw_frames_ctx (checked at
			// load); the context takes the new reference and frees it.
			unsafe {
				let reference = (api.av_buffer_ref)(session.pool);
				layout::write::<Ptr>(ctx, offset, reference);
			}
		}
		session.amf = self.spec.family() == "amf";
		let ret = {
			let _amf = session.amf.then(amf_lock);
			// SAFETY: a configured context and its codec; no options dictionary.
			unsafe { (api.avcodec_open2)(ctx, codec, std::ptr::null_mut()) }
		};
		if ret < 0 {
			return Err(self.error("open", ret));
		}
		// Buffers for the whole session.
		// SAFETY: allocations; the head fields are set before
		// av_frame_get_buffer as documented.
		unsafe {
			session.packet = (api.av_packet_alloc)();
			session.sw = (api.av_frame_alloc)();
			if session.packet.is_null() || session.sw.is_null() {
				return Err(self.unavailable("out of memory".into()));
			}
			let head = session.sw.cast::<FrameHead>();
			(*head).width = w;
			(*head).height = h;
			(*head).format = sw_format;
			let ret = (api.av_frame_get_buffer)(session.sw, 0);
			if ret < 0 {
				return Err(self.error("frame buffer", ret));
			}
			if self.spec.is_vaapi() {
				session.hw = (api.av_frame_alloc)();
				if session.hw.is_null() {
					return Err(self.unavailable("out of memory".into()));
				}
			}
		}
		tracing::debug!(
			backend = self.spec.name,
			width,
			height,
			fps,
			bitrate,
			low_power,
			"FFmpeg encoder opened"
		);
		Ok(session)
	}

	/// A VA-API surface pool of `sw_format` (an `AVPixelFormat`) for `width`
	/// x `height`.
	/// Copy the I420 picture into the session's frame (cropped to the coded
	/// size; NV12 interleaved where the backend wants it).
	fn fill(&self, session: &Session, frame: &VideoFrame) -> Result<()> {
		let FrameData::I420 { y, u, v } = &frame.data else {
			unreachable!("converted to I420 before");
		};
		let api = &self.ffmpeg.api;
		// SAFETY: the frame was allocated by av_frame_get_buffer for the
		// session size and format; make_writable keeps that and only swaps
		// buffers the encoder still references.
		let ret = unsafe { (api.av_frame_make_writable)(session.sw) };
		if ret < 0 {
			return Err(self.error("frame buffer", ret));
		}
		let (w, h) = (session.width as usize, session.height as usize);
		let (cw, ch) = (w / 2, h / 2);
		// SAFETY: planes of the session's frame: `linesize` bytes per row,
		// `h` (luma) or `h / 2` (chroma) rows, at least `w` (or the chroma
		// width) bytes each, owned by the frame (writable, see above).
		unsafe {
			let head = &*session.sw.cast::<FrameHead>();
			let plane = |i: usize, rows: usize| {
				let stride = head.linesize[i] as usize;
				(std::slice::from_raw_parts_mut(head.data[i], stride * rows), stride)
			};
			let (dst, stride) = plane(0, h);
			for row in 0..h {
				dst[row * stride..][..w].copy_from_slice(y.row(row, w));
			}
			if session.nv12 {
				let (dst, stride) = plane(1, ch);
				for row in 0..ch {
					let out = &mut dst[row * stride..][..cw * 2];
					let (u_row, v_row) = (u.row(row, cw), v.row(row, cw));
					for ((pair, &u), &v) in out.chunks_exact_mut(2).zip(u_row).zip(v_row) {
						pair[0] = u;
						pair[1] = v;
					}
				}
			} else {
				for (i, src) in [(1, u), (2, v)] {
					let (dst, stride) = plane(i, ch);
					for row in 0..ch {
						dst[row * stride..][..cw].copy_from_slice(src.row(row, cw));
					}
				}
			}
		}
		Ok(())
	}

	/// Receive every packet that is ready.
	fn drain(&self, session: &mut Session, out: &mut dyn FnMut(EncodedChunk<'_>)) -> Result<bool> {
		let api = &self.ffmpeg.api;
		let mut keyframe = false;
		loop {
			// SAFETY: a live context and packet.
			let ret = unsafe { (api.avcodec_receive_packet)(session.ctx, session.packet) };
			if ret == sys::EAGAIN || ret == sys::EOF {
				return Ok(keyframe);
			}
			if ret < 0 {
				return Err(self.error("receive", ret));
			}
			// SAFETY: a packet just filled by the encoder: `data` holds
			// `size` bytes until av_packet_unref.
			unsafe {
				let head = &*session.packet.cast::<PacketHead>();
				let key = head.flags & sys::PKT_FLAG_KEY != 0 || session.first_packet;
				session.first_packet = false;
				keyframe |= key;
				let pts_90khz = session
					.timestamps
					.take(head.pts)
					.unwrap_or_else(|| (head.pts.max(0) as u64) * 90_000 / u64::from(session.fps));
				if !head.data.is_null() && head.size > 0 {
					let data = std::slice::from_raw_parts(head.data, head.size as usize);
					out(EncodedChunk { data, keyframe: key, pts_90khz });
				}
				(api.av_packet_unref)(session.packet);
			}
		}
	}

	/// Send one frame (or NULL to flush), draining packets as the encoder
	/// asks.
	fn send(
		&self,
		session: &mut Session,
		frame: Ptr,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<bool> {
		if frame.is_null() && !session.sent {
			// Nothing to flush, and FFmpeg 9's VA-API encoders crash when
			// drained before their first frame (h264_vaapi on an RX 7900
			// GRE: SIGSEGV in avcodec_send_frame), which is what dropping an
			// encoder whose first frame failed (a DMA-BUF import, a surface
			// upload) used to do.
			return Ok(false);
		}
		let api = &self.ffmpeg.api;
		let mut keyframe = false;
		loop {
			// SAFETY: a live context; `frame` is NULL or a live frame.
			let ret = unsafe { (api.avcodec_send_frame)(session.ctx, frame) };
			if ret == sys::EAGAIN {
				keyframe |= self.drain(session, out)?;
				continue;
			}
			if ret < 0 && ret != sys::EOF {
				return Err(self.error("send", ret));
			}
			session.sent |= !frame.is_null();
			break;
		}
		keyframe |= self.drain(session, out)?;
		Ok(keyframe)
	}

	/// The session for a `width` x `height` frame, opened anew when the size
	/// changed or a reinit is due; whether the frame must be a keyframe.
	fn prepare(
		&mut self,
		width: u32,
		height: u32,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<(Session, bool)> {
		let (w, h) = self.coded_size(width, height);
		let mut force = force_keyframe;
		let reopen = match &self.session {
			None => true,
			Some(s) => {
				(s.width, s.height) != (w, h)
					|| self.reinit == Reinit::Now
					|| (self.reinit == Reinit::AtKeyframe && force)
			}
		};
		if reopen {
			// Frames the old session still holds (encoders with a delay) come
			// out first; then one session at a time (hardware encoders have
			// few).
			if let Some(mut old) = self.session.take()
				&& let Err(e) = self.send(&mut old, std::ptr::null_mut(), out)
			{
				tracing::debug!(backend = self.spec.name, "flushing the old session: {e}");
			}
			self.session = Some(self.open(w, h)?);
			self.reinit = Reinit::No;
			force = true;
		}
		Ok((self.session.take().expect("opened above"), force))
	}

	fn encode_i420(
		&mut self,
		frame: &VideoFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let (mut session, force) = self.prepare(frame.width, frame.height, force_keyframe, out)?;
		let result = self.encode_in(&mut session, frame, force, out);
		self.session = Some(session);
		result
	}

	/// Encode a frame that is still in a DMA-BUF, without the CPU reading
	/// it: the buffer becomes a VA-API surface (`av_hwframe_map` of a DRM
	/// PRIME frame).
	///
	/// VA-API backends only. NV12 buffers go to the encoder as they are
	/// (their size must be one it encodes exactly), and stay in use until
	/// this frame's packet came out (the encoder reads them one frame deep).
	/// RGB buffers (`XR24`, `AR24`, `XB24`, `AB24`: what screen capture
	/// delivers) are converted to NV12 on the GPU ([`GpuConverter`]), cropped
	/// to the coded size, and are no longer read when this returns.
	///
	/// Anything else fails with `Error::CodecUnavailable`; callers then map
	/// the buffer and use [`encode_with`](VideoEncoder::encode_with).
	pub fn encode_dmabuf(
		&mut self,
		frame: &DmaBufRef,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		if !self.spec.is_vaapi() {
			return Err(self.unavailable(format!("{} does not import DMA-BUFs", self.spec.name)));
		}
		if RGB_FOURCCS.contains(&frame.fourcc) && frame.plane_count == 1 {
			let converter = match self
				.gpu
				.get_or_insert_with(|| GpuConverter::new().map_err(|e| e.to_string()))
			{
				Ok(converter) => converter,
				Err(e) => {
					return Err(Error::CodecUnavailable {
						codec: self.spec.codec,
						reason: e.clone(),
					});
				}
			};
			let layer = GpuLayer {
				size: (frame.width, frame.height),
				alignment: self.alignment,
				due: true,
			};
			let mut converted = [None];
			if let Err(e) = converter.convert(frame, &[layer], &mut converted) {
				return Err(self.unavailable(e.to_string()));
			}
			let gpu = converted[0].take().expect("a due layer gets a frame");
			return self.encode_gpu(&gpu, force_keyframe, out);
		}
		if frame.fourcc != drm_fourcc(b"NV12") || frame.plane_count != 2 {
			return Err(self.unavailable("only NV12 and RGB DMA-BUFs are imported".into()));
		}
		if self.coded_size(frame.width, frame.height) != (frame.width, frame.height) {
			return Err(self.unavailable(format!(
				"{}x{} is not a size {} encodes exactly",
				frame.width, frame.height, self.spec.name
			)));
		}
		let refs = self.ffmpeg.frame_refs.clone().map_err(|e| self.unavailable(e))?;
		let (mut session, force) = self.prepare(frame.width, frame.height, force_keyframe, out)?;
		let result = self.import_and_send(&mut session, frame, force, refs, out);
		self.session = Some(session);
		result
	}

	/// Map an NV12 DMA-BUF onto a surface of the session and encode it.
	fn import_and_send(
		&mut self,
		session: &mut Session,
		frame: &DmaBufRef,
		force: bool,
		refs: layout::FrameRefs,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let api = &self.ffmpeg.api;
		if !self.dmabuf_checked {
			// The table's AVFrame.hw_frames_ctx must be where FFmpeg puts
			// the pool of a surface it allocates.
			// SAFETY: `hw` is our frame, the pool the session's.
			let ok = unsafe {
				let ret = (api.av_hwframe_get_buffer)(session.pool, session.hw, 0);
				let ok = ret >= 0 && layout::check_hw_frames_ctx(session.hw, session.pool, refs);
				(api.av_frame_unref)(session.hw);
				ok
			};
			if !ok {
				return Err(self.unavailable("AVFrame.hw_frames_ctx check failed".into()));
			}
			self.dmabuf_checked = true;
		}
		let hw = session.hw;
		gpu::map_dmabuf(self.ffmpeg, frame, session.pool, hw, refs).map_err(|e| {
			Error::Encoder { codec: self.spec.codec, message: format!("{}: {e}", self.spec.name) }
		})?;
		let pts =
			self.stamp(session, frame.timestamp, (frame.timestamp.as_micros() * 9 / 100) as u64);
		self.send_surface(session, pts, force, out)
	}

	/// Encode the surface in `session.hw` (taken from the session's frame
	/// afterwards; the encoder keeps its own reference while it needs it).
	fn send_surface(
		&self,
		session: &mut Session,
		pts: i64,
		force: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let hw = session.hw;
		let pict_type = if force { sys::PICTURE_TYPE_I } else { sys::PICTURE_TYPE_NONE };
		// SAFETY: offsets checked at load for this release's AVFrame; `hw`
		// holds a surface until the unref.
		unsafe {
			layout::write::<i64>(hw, self.ffmpeg.frame.pts, pts);
			layout::write::<c_int>(hw, self.ffmpeg.frame.pict_type, pict_type);
		}
		let result = self.send(session, hw, out);
		// SAFETY: our frame.
		unsafe { (self.ffmpeg.api.av_frame_unref)(hw) };
		result.map(|_| ())
	}

	/// The pts of a frame taken at `timestamp`: frame numbers at the
	/// session's rate (its time base), strictly increasing; remembered with
	/// the 90 kHz time for the packets.
	fn stamp(&self, session: &mut Session, timestamp: Duration, pts_90khz: u64) -> i64 {
		let exact = timestamp.as_secs_f64() * f64::from(session.fps);
		let mut pts = exact.round() as i64;
		if let Some(last) = session.last_pts
			&& pts <= last
		{
			pts = last + 1;
		}
		session.last_pts = Some(pts);
		session.timestamps.push(pts, pts_90khz);
		pts
	}

	fn encode_in(
		&self,
		session: &mut Session,
		frame: &VideoFrame,
		force: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let api = &self.ffmpeg.api;
		self.fill(session, frame)?;
		let pts = self.stamp(session, frame.timestamp, frame.pts_90khz());
		let fields = self.ffmpeg.frame;
		let pict_type = if force { sys::PICTURE_TYPE_I } else { sys::PICTURE_TYPE_NONE };
		let input = if session.hw.is_null() {
			session.sw
		} else {
			// SAFETY: `hw` is our frame (unreferenced after each send); the
			// pool is the session's; transfer copies the planes into the
			// surface.
			unsafe {
				let ret = (api.av_hwframe_get_buffer)(session.pool, session.hw, 0);
				if ret < 0 {
					return Err(self.error("VA-API surface", ret));
				}
				let ret = (api.av_hwframe_transfer_data)(session.hw, session.sw, 0);
				if ret < 0 {
					(api.av_frame_unref)(session.hw);
					return Err(self.error("VA-API upload", ret));
				}
			}
			session.hw
		};
		// SAFETY: offsets checked at load for this release's AVFrame.
		unsafe {
			layout::write::<i64>(input, fields.pts, pts);
			layout::write::<c_int>(input, fields.pict_type, pict_type);
		}
		let result = self.send(session, input, out);
		if !session.hw.is_null() {
			// The encoder holds its own reference; the surface returns to the
			// pool once it is done with it.
			// SAFETY: our frame.
			unsafe { (api.av_frame_unref)(session.hw) };
		}
		result.map(|_| ())
	}

	/// Encode one frame and flush the encoder (the self-test), which has to
	/// give a keyframe; the size an AV1 stream declares.
	fn self_test(&mut self, width: u32, height: u32) -> Result<Option<(u32, u32)>> {
		let frame = test_frame(width, height);
		let mut packets = 0;
		let mut keyframe = false;
		let mut declared = None;
		let av1 = self.spec.codec == Codec::Av1;
		let mut take = |chunk: EncodedChunk<'_>| {
			packets += 1;
			keyframe |= chunk.keyframe;
			if declared.is_none() && av1 {
				declared = av1_frame_size(chunk.data);
			}
		};
		let encoded = self.encode_with(&frame, true, &mut take);
		let flushed = encoded.and_then(|()| {
			let mut session = self.session.take().expect("opened by the frame");
			self.send(&mut session, std::ptr::null_mut(), &mut take)
		});
		flushed?;
		match (packets, keyframe) {
			(0, _) => Err(self.unavailable("no packet from the test frame".into())),
			(_, false) => {
				Err(self.unavailable("the test frame did not come out as a keyframe".into()))
			}
			_ => Ok(declared),
		}
	}
}

impl Drop for FfmpegEncoder {
	/// Ends the stream properly (SVT-AV1 complains otherwise); the frames
	/// still inside are dropped.
	fn drop(&mut self) {
		if let Some(mut session) = self.session.take() {
			let _ = self.send(&mut session, std::ptr::null_mut(), &mut |_| {});
		}
	}
}

impl VideoEncoder for FfmpegEncoder {
	fn codec(&self) -> Codec {
		self.spec.codec
	}

	fn backend(&self) -> EncoderBackend {
		EncoderBackend::Ffmpeg(self.spec.name)
	}

	fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
		let mut frames = Vec::new();
		self.encode_with(frame, force_keyframe, &mut |f| {
			frames.push(EncodedFrame {
				data: f.data.to_vec(),
				keyframe: f.keyframe,
				pts_90khz: f.pts_90khz,
			});
		})?;
		Ok(frames)
	}

	/// Copies the picture into the session's reused frame (or VA-API
	/// surface) and hands out FFmpeg's packet buffer.
	fn encode_with(
		&mut self,
		frame: &VideoFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let i420 = convert::to_i420(frame)?;
		self.encode_i420(&i420, force_keyframe, out)
	}

	fn set_bitrate(&mut self, bps: u32) -> Result<()> {
		let bps = bps.max(1);
		self.config.bitrate_bps = bps;
		let Some((ctx, running, opened)) =
			self.session.as_ref().map(|s| (s.ctx, s.bitrate, s.opened))
		else {
			return Ok(());
		};
		if running == bps {
			return Ok(());
		}
		if self.spec.dynamic_bitrate {
			// The wrapper compares these with its running configuration
			// before each frame and reconfigures the encoder.
			for name in ["b", "maxrate", "bufsize"] {
				let ret = self.set_int(ctx, name, i64::from(bps));
				if ret < 0 {
					return Err(self.error(name, ret));
				}
			}
			if let Some(session) = &mut self.session {
				session.bitrate = bps;
			}
		} else if u64::from(bps) * 2 < u64::from(running) {
			// Congestion: waiting for a keyframe would keep overshooting.
			self.reinit = Reinit::Now;
		} else if u64::from(bps) * 2 > u64::from(running) * 3
			&& opened.elapsed() >= REOPEN_FOR_BITRATE
		{
			// Much more room (the estimate ramping up after the start): a
			// screen that never asks for a keyframe would otherwise keep
			// the start bitrate.
			self.reinit = Reinit::Now;
		} else {
			self.reinit = self.reinit.max(Reinit::AtKeyframe);
		}
		Ok(())
	}

	/// The time base is the frame rate, fixed per session: a new rate opens
	/// a new session (a keyframe).
	fn set_fps(&mut self, fps: u32) -> Result<()> {
		let fps = fps.max(1);
		self.config.fps = fps;
		if self.session.as_ref().is_some_and(|s| s.fps != fps) {
			self.reinit = Reinit::Now;
		}
		Ok(())
	}

	/// VA-API encoders take NV12 surfaces of the sizes they encode exactly.
	fn gpu_alignment(&self) -> Option<(u32, u32)> {
		self.spec.is_vaapi().then_some(self.alignment)
	}

	/// The surface goes to the encoder as it is: the encoder takes a
	/// reference, so it stays out of its pool until the encoder is done.
	fn encode_gpu(
		&mut self,
		frame: &GpuFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		if !self.spec.is_vaapi() {
			return Err(self.unavailable(format!("{} takes no VA-API surfaces", self.spec.name)));
		}
		if frame.surface.0.is_null() {
			return Err(self.unavailable("a GPU frame without a surface (out of memory)".into()));
		}
		if self.coded_size(frame.width, frame.height) != (frame.width, frame.height) {
			return Err(self.unavailable(format!(
				"{}x{} is not a size {} encodes exactly",
				frame.width, frame.height, self.spec.name
			)));
		}
		let (mut session, force) = self.prepare(frame.width, frame.height, force_keyframe, out)?;
		let pts = self.stamp(&mut session, frame.timestamp, frame.pts_90khz());
		// SAFETY: `hw` is our frame, unreferenced after each frame;
		// `surface` is a live VA-API frame of the same device.
		let ret = unsafe { (self.ffmpeg.api.av_frame_ref)(session.hw, frame.surface.0) };
		let result = if ret < 0 {
			Err(self.error("GPU frame", ret))
		} else {
			self.send_surface(&mut session, pts, force, out)
		};
		self.session = Some(session);
		result
	}
}

/// A gradient test picture.
fn test_frame(width: u32, height: u32) -> VideoFrame {
	let mut frame = VideoFrame::black_i420(width, height);
	if let FrameData::I420 { y, .. } = &mut frame.data {
		for (i, px) in y.data.iter_mut().enumerate() {
			*px = 16 + ((i % width as usize) * 200 / width as usize) as u8;
		}
	}
	frame.with_timestamp(Duration::ZERO)
}

/// A backend and the result of its self-test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendStatus {
	pub spec: &'static BackendSpec,
	/// `Ok` if it encoded the test frame, else why not.
	pub available: std::result::Result<(), String>,
	/// How long its self-test took (the probe runs them in parallel, so the
	/// startup cost is the slowest one).
	pub took: Duration,
	/// The sizes it encodes exactly are multiples of these; frames are
	/// cropped to them. (2, 2) for most; AV1 on an RDNA3 GPU is (64, 16):
	/// it pads anything else (1366x768 comes out as 1408x768, 1600x900 as
	/// 1600x912, 1080 rows as 1082), and AV1 has no cropping like H.264's
	/// SPS (its render size is a display hint decoders do not apply), so a
	/// viewer would show the padding.
	pub alignment: (u32, u32),
}

/// Size of the self-test frame.
const TEST_SIZE: (u32, u32) = (320, 240);

/// Size of the AV1 self-test frame: 2 more than a power of two, so its next
/// multiple of 4, 8, ... 256 is a different size each, and the size the
/// stream declares tells which alignment the encoder pads to.
///
/// Not a multiple of 8: the RDNA3 encoder pads a height that is 8 more than
/// a multiple of 16 by 2 rows only (1080 to 1082), any other by up to 16.
const AV1_TEST_SIZE: (u32, u32) = (258, 258);

/// The alignment that turns `tested` into the `declared` size: the power of
/// two whose next multiple of `tested` is `declared` (2 if they are equal,
/// or if no power of two explains it, which leaves frames uncropped).
fn alignment_of(tested: u32, declared: u32) -> u32 {
	(2..=8).map(|s| 1 << s).find(|a| tested.next_multiple_of(*a) == declared).unwrap_or(2)
}

fn test_backend(spec: &'static BackendSpec) -> std::result::Result<(u32, u32), String> {
	let config = EncoderConfig { fps: 30, bitrate_bps: 500_000, ..EncoderConfig::default() };
	let mut encoder = FfmpegEncoder::new(spec.name, config).map_err(|e| e.to_string())?;
	let (w, h) = if spec.codec == Codec::Av1 { AV1_TEST_SIZE } else { TEST_SIZE };
	match encoder.self_test(w, h) {
		Ok(Some((dw, dh))) => Ok((alignment_of(w, dw), alignment_of(h, dh))),
		Ok(None) => Ok((2, 2)),
		Err(Error::CodecUnavailable { reason, .. }) => Err(reason),
		Err(Error::Encoder { message, .. }) => Err(message),
		Err(e) => Err(e.to_string()),
	}
}

/// The frame size an AV1 stream declares: `max_frame_width_minus_1 + 1` and
/// `max_frame_height_minus_1 + 1` of the first sequence header OBU in
/// `data` (AV1 specification 5.3 and 5.5), or `None` if there is none.
fn av1_frame_size(data: &[u8]) -> Option<(u32, u32)> {
	const OBU_SEQUENCE_HEADER: u8 = 1;
	let mut rest = data;
	while let [header, tail @ ..] = rest {
		let mut body = if header & 0x04 != 0 { tail.get(1..)? } else { tail };
		let size = if header & 0x02 != 0 {
			let mut size = 0usize;
			let mut i = 0;
			loop {
				let byte = *body.get(i)?;
				size |= usize::from(byte & 0x7f) << (7 * i);
				i += 1;
				if byte & 0x80 == 0 {
					break;
				}
				if i == 8 {
					return None;
				}
			}
			body = &body[i..];
			size
		} else {
			body.len()
		};
		let payload = body.get(..size)?;
		if (header >> 3) & 0xf == OBU_SEQUENCE_HEADER {
			return av1_sequence_size(payload);
		}
		rest = &body[size..];
	}
	None
}

/// `sequence_header_obu()` up to the maximum frame size.
fn av1_sequence_size(payload: &[u8]) -> Option<(u32, u32)> {
	let mut pos = 0usize;
	let mut read = |bits: u32| -> Option<u32> {
		(0..bits).try_fold(0u32, |value, _| {
			let bit = (payload.get(pos / 8)? >> (7 - pos % 8)) & 1;
			pos += 1;
			Some(value << 1 | u32::from(bit))
		})
	};
	read(3)?; // seq_profile
	read(1)?; // still_picture
	if read(1)? == 1 {
		// reduced_still_picture_header
		read(5)?; // seq_level_idx[0]
	} else {
		let mut decoder_model = false;
		let mut delay_bits = 0;
		if read(1)? == 1 {
			// timing_info: display tick, time scale, equal_picture_interval
			read(32)?;
			read(32)?;
			if read(1)? == 1 {
				// num_ticks_per_picture_minus_1, uvlc()
				let mut zeros = 0;
				while read(1)? == 0 {
					zeros += 1;
				}
				if zeros < 32 {
					read(zeros)?;
				}
			}
			decoder_model = read(1)? == 1;
			if decoder_model {
				delay_bits = read(5)? + 1; // buffer_delay_length_minus_1
				read(32)?; // num_units_in_decoding_tick
				read(10)?; // buffer_removal_time / frame_presentation_time lengths
			}
		}
		let initial_display_delay = read(1)? == 1;
		for _ in 0..=read(5)? {
			read(12)?; // operating_point_idc
			if read(5)? > 7 {
				read(1)?; // seq_tier
			}
			if decoder_model && read(1)? == 1 {
				// operating_parameters_info: both buffer delays, low_delay_mode_flag
				read(delay_bits)?;
				read(delay_bits)?;
				read(1)?;
			}
			if initial_display_delay && read(1)? == 1 {
				read(4)?;
			}
		}
	}
	let (width_bits, height_bits) = (read(4)? + 1, read(4)? + 1);
	Some((read(width_bits)? + 1, read(height_bits)? + 1))
}

/// Whether FFmpeg has `name` at all (cheap, before any self-test).
fn in_build(ffmpeg: &Ffmpeg, name: &str) -> bool {
	let name = cstr(name);
	// SAFETY: a C string; returns a static codec or NULL.
	!unsafe { (ffmpeg.api.avcodec_find_encoder_by_name)(name.as_ptr()) }.is_null()
}

/// PCI vendor of a GPU, as `/sys/class/drm/*/device/vendor` gives it.
#[cfg(target_os = "linux")]
mod vendor {
	pub const AMD: u32 = 0x1002;
	pub const NVIDIA: u32 = 0x10de;
	pub const INTEL: u32 = 0x8086;
}

/// The PCI vendors of this machine's DRM render nodes, read once; empty if
/// `/sys/class/drm` says nothing (a container without it, say), which is
/// taken as "unknown" and skips nothing.
#[cfg(target_os = "linux")]
fn drm_vendors() -> &'static [u32] {
	static VENDORS: OnceLock<Vec<u32>> = OnceLock::new();
	VENDORS.get_or_init(|| {
		let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
		let mut found: Vec<u32> = entries
			.flatten()
			.filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
			.filter_map(|e| std::fs::read_to_string(e.path().join("device/vendor")).ok())
			.filter_map(|text| u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok())
			.collect();
		found.sort_unstable();
		found.dedup();
		found
	})
}

/// Sends the process's standard error to a temporary file while it lives,
/// and logs whatever landed there when it is dropped.
///
/// FFmpeg's own messages already go to `tracing` (see
/// [`Ffmpeg::capture_log`](super::Ffmpeg::capture_log)), but the libraries
/// behind the encoders write to the descriptor themselves and cannot be
/// asked not to: SVT-AV1 prints a build banner and its allocation totals,
/// AMD's AMF runtime prints `GetProperty(...) not found` warnings, and Mesa
/// prints a `RADV_PERFTEST` deprecation notice when AMF brings up Vulkan.
/// Opening every encoder at startup meant about thirty such lines on every
/// run of the app.
///
/// This moves the descriptor, so anything else the process writes to
/// standard error meanwhile lands in the file too and comes back as one
/// `debug` record. `VOELIN_FFMPEG_PROBE_STDERR=1` leaves it alone.
struct QuietStderr {
	#[cfg(target_os = "linux")]
	saved: std::os::fd::OwnedFd,
	#[cfg(target_os = "linux")]
	file: std::path::PathBuf,
}

impl QuietStderr {
	#[cfg(target_os = "linux")]
	fn start() -> Option<Self> {
		use std::os::fd::AsFd;

		if std::env::var_os("VOELIN_FFMPEG_PROBE_STDERR").is_some() {
			return None;
		}
		let file =
			std::env::temp_dir().join(format!("voelin-encoder-probe-{}.log", std::process::id()));
		let sink = std::fs::File::create(&file).ok()?;
		let stderr = std::io::stderr();
		let saved = rustix::io::dup(stderr.as_fd()).ok()?;
		// Anything already buffered belongs on the real descriptor.
		let _ = std::io::Write::flush(&mut std::io::stderr());
		rustix::stdio::dup2_stderr(sink.as_fd()).ok()?;
		Some(Self { saved, file })
	}

	#[cfg(not(target_os = "linux"))]
	fn start() -> Option<Self> {
		None
	}
}

impl Drop for QuietStderr {
	fn drop(&mut self) {
		#[cfg(target_os = "linux")]
		{
			use std::os::fd::AsFd;

			let _ = std::io::Write::flush(&mut std::io::stderr());
			let _ = rustix::stdio::dup2_stderr(self.saved.as_fd());
			if let Ok(text) = std::fs::read_to_string(&self.file) {
				let text = text.trim();
				if !text.is_empty() {
					tracing::debug!(target: "ffmpeg", "the encoder libraries wrote:\n{text}");
				}
			}
			let _ = std::fs::remove_file(&self.file);
		}
	}
}

/// Whether the hardware `spec` needs can be in this machine at all.
///
/// A self-test of a family whose vendor is absent still costs 0.4-1.7 s of
/// driver initialisation before it fails (measured here: `av1_nvenc` 1.7 s,
/// `vp9_qsv` 1.2 s), and the probe's wall time is its slowest test. Nothing
/// is skipped when the vendors cannot be read, so an unusual setup only
/// pays the time it used to.
fn vendor_may_be_present(spec: &BackendSpec) -> std::result::Result<(), String> {
	#[cfg(target_os = "linux")]
	{
		let vendors = drm_vendors();
		if vendors.is_empty() {
			return Ok(());
		}
		let (wanted, what) = match spec.family() {
			"nvenc" => (vendor::NVIDIA, "no NVIDIA GPU"),
			"qsv" => (vendor::INTEL, "no Intel GPU"),
			"amf" => (vendor::AMD, "no AMD GPU"),
			_ => return Ok(()),
		};
		// NVIDIA's driver can run without a DRM node (`nvidia-drm.modeset=0`).
		if wanted == vendor::NVIDIA && std::path::Path::new("/dev/nvidiactl").exists() {
			return Ok(());
		}
		if !vendors.contains(&wanted) {
			return Err(format!("{what} in this machine"));
		}
	}
	let _ = spec;
	Ok(())
}

/// Why AMF is not used here, if it is not.
///
/// On Linux it is used only with `VOELIN_FFMPEG_AMF=1`: VA-API drives the
/// same AMD encoder there, measured as fast with less CPU, and only VA-API
/// takes DMA-BUFs without a copy. AMF on Linux is AMD's closed runtime on
/// top of Vulkan, and it has crashed the app inside `avcodec_open2` (see
/// [`amf_lock`]); a library that is never loaded cannot.
fn amf_refused() -> Option<&'static str> {
	let opted_in = std::env::var_os("VOELIN_FFMPEG_AMF").is_some_and(|v| v != "0");
	(cfg!(target_os = "linux") && !opted_in).then_some(
		"not used on Linux: VA-API drives the same AMD encoder (VOELIN_FFMPEG_AMF=1 uses it)",
	)
}

/// The result of [`probe`], once it ran.
static PROBE: OnceLock<Vec<BackendStatus>> = OnceLock::new();

/// Every backend with its self-test result (run once per process, the
/// backends in parallel). Empty without FFmpeg.
///
/// A backend whose vendor is not in the machine is not opened at all
/// ([`vendor_may_be_present`]): its driver would spend up to 1.7 s failing,
/// and the probe costs as much as its slowest test.
pub fn probe() -> &'static [BackendStatus] {
	PROBE.get_or_init(|| {
		let Ok(ffmpeg) = Ffmpeg::get() else { return Vec::new() };
		let started = Instant::now();
		let quiet = QuietStderr::start();
		let statuses: Vec<BackendStatus> = std::thread::scope(|scope| {
			let tests: Vec<_> = BACKENDS
				.iter()
				.map(|spec| {
					let absent = vendor_may_be_present(spec).err();
					let present = absent.is_none() && in_build(ffmpeg, spec.name);
					let test = present.then(|| {
						scope.spawn(move || {
							let started = Instant::now();
							(test_backend(spec), started.elapsed())
						})
					});
					(spec, absent, test)
				})
				.collect();
			tests
				.into_iter()
				.map(|(spec, absent, test)| {
					let (tested, took) = match test {
						None => (
							Err(absent.unwrap_or_else(|| "not in this FFmpeg build".into())),
							Duration::ZERO,
						),
						Some(handle) => handle.join().unwrap_or_else(|_| {
							(Err("the self-test panicked".into()), Duration::ZERO)
						}),
					};
					let alignment = *tested.as_ref().unwrap_or(&(2, 2));
					BackendStatus { spec, available: tested.map(|_| ()), took, alignment }
				})
				.collect()
		});
		// Standard error comes back (and what the libraries wrote is logged)
		// before our own report.
		drop(quiet);
		for status in &statuses {
			match &status.available {
				Ok(()) => tracing::info!(
					backend = status.spec.name,
					ms = status.took.as_millis() as u64,
					alignment = ?status.alignment,
					"FFmpeg encoder available"
				),
				Err(e) => tracing::debug!(
					backend = status.spec.name,
					ms = status.took.as_millis() as u64,
					"FFmpeg encoder unusable: {e}"
				),
			}
		}
		tracing::info!(
			ms = started.elapsed().as_millis() as u64,
			tested = statuses.iter().filter(|s| !s.took.is_zero()).count(),
			"FFmpeg encoders probed"
		);
		statuses
	})
}

/// Creates encoders of one backend that passed its self-test.
pub struct FfmpegFactory {
	spec: &'static BackendSpec,
}

impl FfmpegFactory {
	/// Factories of the available backends, in [`BACKENDS`] order.
	pub fn available() -> Vec<FfmpegFactory> {
		probe()
			.iter()
			.filter(|s| s.available.is_ok())
			.map(|s| FfmpegFactory { spec: s.spec })
			.collect()
	}

	pub fn spec(&self) -> &'static BackendSpec {
		self.spec
	}
}

impl EncoderFactory for FfmpegFactory {
	fn name(&self) -> &'static str {
		self.spec.name
	}

	fn codecs(&self) -> Vec<Codec> {
		vec![self.spec.codec]
	}

	fn create(&self, codec: Codec, config: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
		if codec != self.spec.codec {
			return Err(Error::CodecUnavailable {
				codec,
				reason: format!("{} encodes {}", self.spec.name, self.spec.codec),
			});
		}
		Ok(Box::new(FfmpegEncoder::new(self.spec.name, config.clone())?))
	}

	fn is_hardware(&self) -> bool {
		self.spec.is_hardware()
	}

	fn is_automatic(&self) -> bool {
		self.spec.is_automatic()
	}

	fn backend(&self) -> EncoderBackend {
		EncoderBackend::Ffmpeg(self.spec.name)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn backend_table() {
		for (i, spec) in BACKENDS.iter().enumerate() {
			assert!(BACKENDS[..i].iter().all(|b| b.name != spec.name), "{} twice", spec.name);
			assert_eq!(BackendSpec::by_name(spec.name), Some(spec));
			// VA-API needs the hardware frame pool; nothing else does.
			assert_eq!(spec.is_vaapi(), spec.name.ends_with("_vaapi"), "{}", spec.name);
			assert_eq!(spec.is_hardware(), !spec.name.starts_with("lib"), "{}", spec.name);
		}
		assert_eq!(BackendSpec::by_name("h264_vaapi").unwrap().family(), "vaapi");
		assert_eq!(BackendSpec::by_name("libx264").unwrap().family(), "libx264");
	}

	#[test]
	fn timestamps_follow_packets() {
		let mut t = Timestamps::new();
		t.push(0, 0);
		t.push(1, 3000);
		t.push(3, 9000);
		assert_eq!(t.take(1), Some(3000), "an older entry is skipped");
		assert_eq!(t.take(2), None);
		assert_eq!(t.take(3), Some(9000));
		for i in 0..100 {
			t.push(i, i as u64);
		}
		assert_eq!(t.pending.len(), 64, "bounded");
	}

	/// [`h264_level`] must agree with the level the signalling offers
	/// (`voelin_stream::h264::level_idc`) for every stream, or the SPS and
	/// the SDP would disagree again. Two copies of ITU-T Table A-1, pinned
	/// to each other: this crate cannot depend on voelin-stream (it is the
	/// lower one), and voelin-stream must not pull in the codecs.
	#[test]
	fn levels_agree_with_the_signalling() {
		for (w, h) in [
			(320u32, 240u32),
			(640, 360),
			(1280, 720),
			(1920, 1080),
			(2560, 1440),
			(3840, 2160),
			(4096, 64),
			(16384, 16384),
		] {
			for fps in [1u32, 15, 30, 60, 120] {
				for kbps in [200u32, 4_000, 8_000, 30_000] {
					for (mine, theirs) in [
						(H264Profile::ConstrainedHigh, voelin_stream::H264Profile::ConstrainedHigh),
						(
							H264Profile::ConstrainedBaseline,
							voelin_stream::H264Profile::ConstrainedBaseline,
						),
					] {
						let bitrate = kbps * 1000;
						let ours = h264_level(mine, w, h, fps, bitrate);
						let signalled =
							voelin_stream::h264::level_idc(theirs, w, h, fps, u64::from(bitrate));
						assert_eq!(
							ours,
							i64::from(signalled),
							"{w}x{h}@{fps} {kbps}k {mine:?}: encoder level {ours}, offer \
							 {signalled}"
						);
					}
				}
			}
		}
	}

	/// The frame size of a sequence header that takes every optional branch
	/// (timing info, decoder model, two operating points), behind a temporal
	/// delimiter, and the alignment derived from it.
	#[test]
	fn av1_declared_size_and_alignment() {
		// (value, bits), most significant bit first.
		let fields: &[(u32, u32)] = &[
			(0, 3),      // seq_profile
			(0, 1),      // still_picture
			(0, 1),      // reduced_still_picture_header
			(1, 1),      // timing_info_present_flag
			(1, 32),     // num_units_in_display_tick
			(60, 32),    // time_scale
			(1, 1),      // equal_picture_interval
			(0b011, 3),  // num_ticks_per_picture_minus_1 = 2, uvlc
			(1, 1),      // decoder_model_info_present_flag
			(9, 5),      // buffer_delay_length_minus_1
			(1, 32),     // num_units_in_decoding_tick
			(0, 10),     // the two time lengths
			(1, 1),      // initial_display_delay_present_flag
			(1, 5),      // operating_points_cnt_minus_1
			(0x103, 12), // operating_point_idc[0]
			(9, 5),      // seq_level_idx[0], > 7
			(0, 1),      // seq_tier[0]
			(1, 1),      // decoder_model_present_for_this_op[0]
			(500, 10),   // decoder_buffer_delay
			(500, 10),   // encoder_buffer_delay
			(0, 1),      // low_delay_mode_flag
			(1, 1),      // initial_display_delay_present_for_this_op[0]
			(9, 4),      // initial_display_delay_minus_1[0]
			(0x001, 12), // operating_point_idc[1]
			(4, 5),      // seq_level_idx[1]
			(0, 1),      // decoder_model_present_for_this_op[1]
			(0, 1),      // initial_display_delay_present_for_this_op[1]
			(10, 4),     // frame_width_bits_minus_1
			(10, 4),     // frame_height_bits_minus_1
			(1919, 11),  // max_frame_width_minus_1
			(1081, 11),  // max_frame_height_minus_1
			(0xa5, 8),   // what follows
		];
		let mut payload = Vec::new();
		let mut n = 0;
		for &(value, bits) in fields {
			for i in (0..bits).rev() {
				if n % 8 == 0 {
					payload.push(0);
				}
				*payload.last_mut().unwrap() |= (((value >> i) & 1) as u8) << (7 - n % 8);
				n += 1;
			}
		}
		// Temporal delimiter (type 2, empty), then the sequence header
		// (type 1), both with a size field.
		let mut stream = vec![2 << 3 | 2, 0, 1 << 3 | 2, payload.len() as u8];
		stream.extend_from_slice(&payload);
		assert_eq!(av1_frame_size(&stream), Some((1920, 1082)));
		assert_eq!(av1_frame_size(&stream[..stream.len() - 8]), None, "cut short");
		assert_eq!(av1_frame_size(&[2 << 3 | 2, 0]), None, "no sequence header");

		assert_eq!(alignment_of(258, 258), 2);
		assert_eq!(alignment_of(258, 260), 4);
		assert_eq!(alignment_of(258, 272), 16);
		assert_eq!(alignment_of(258, 320), 64);
		assert_eq!(alignment_of(258, 512), 256);
		assert_eq!(alignment_of(258, 4096), 2, "not an alignment: left alone");
	}

	fn usable(name: &str) -> bool {
		probe().iter().any(|s| s.spec.name == name && s.available.is_ok())
	}

	/// A VA-API surface of `sw_format` (holding `picture`, BGRA, if given)
	/// exported as a DRM PRIME DMA-BUF: what a compositor hands over,
	/// tiling modifier and all. The buffer is valid while this lives.
	struct Exported {
		pool: Ptr,
		surface: Ptr,
		drm: Ptr,
		frame: DmaBufRef,
	}

	impl Exported {
		fn new(
			(width, height): (u32, u32),
			sw_format: c_int,
			fourcc: u32,
			picture: Option<&VideoFrame>,
		) -> Self {
			let ffmpeg = Ffmpeg::get().unwrap();
			let api = &ffmpeg.api;
			let pool = vaapi_pool(ffmpeg, width, height, sw_format).expect("a VA-API surface pool");
			// SAFETY: `pool` is a live frames context of this process's
			// VA-API device; the frames are ours and freed by the drop. The
			// exported frame's `data[0]` is the AVDRMFrameDescriptor FFmpeg
			// filled, valid while the frame holds the mapping.
			unsafe {
				let mut exported = Exported {
					pool,
					surface: (api.av_frame_alloc)(),
					drm: (api.av_frame_alloc)(),
					frame: DmaBufRef {
						width,
						height,
						timestamp: Duration::ZERO,
						fourcc,
						modifier: 0,
						fd: -1,
						size: 0,
						planes: [(0, 0); 4],
						plane_count: 0,
					},
				};
				let (surface, drm) = (exported.surface, exported.drm);
				assert!(!surface.is_null() && !drm.is_null());
				let ret = (api.av_hwframe_get_buffer)(pool, surface, 0);
				assert!(ret >= 0, "av_hwframe_get_buffer: {}", api.error_text(ret));
				if let Some(picture) = picture {
					let FrameData::Bgra(pixels) = &picture.data else { panic!("BGRA only") };
					let mut sw = (api.av_frame_alloc)();
					let head = sw.cast::<FrameHead>();
					((*head).width, (*head).height) = (width as c_int, height as c_int);
					(*head).format = sw_format;
					assert!((api.av_frame_get_buffer)(sw, 0) >= 0);
					let stride = (*head).linesize[0] as usize;
					let rows =
						std::slice::from_raw_parts_mut((*head).data[0], stride * height as usize);
					for (y, row) in rows.chunks_exact_mut(stride).enumerate() {
						row[..width as usize * 4]
							.copy_from_slice(pixels.row(y, width as usize * 4));
					}
					let ret = (api.av_hwframe_transfer_data)(surface, sw, 0);
					(api.av_frame_free)(&mut sw);
					assert!(ret >= 0, "upload: {}", api.error_text(ret));
				}
				(*drm.cast::<FrameHead>()).format = ffmpeg.pix.drm_prime.expect("DRM PRIME");
				let ret = (api.av_hwframe_map)(
					drm,
					surface,
					sys::HWFRAME_MAP_READ | sys::HWFRAME_MAP_DIRECT,
				);
				assert!(ret >= 0, "this driver cannot export a VA-API surface as a DMA-BUF");
				let descriptor =
					*(*drm.cast::<FrameHead>()).data[0].cast::<sys::DrmFrameDescriptor>();
				assert_eq!(descriptor.nb_objects, 1, "one buffer object");
				// The driver may describe a surface one plane per layer (NV12
				// as R8 for Y and GR88 for UV) rather than as one layer, which
				// is how a compositor hands it over. Both name the same bytes
				// of the same object, so flatten the layers into planes.
				let object = descriptor.objects[0];
				let frame = &mut exported.frame;
				for layer in &descriptor.layers[..descriptor.nb_layers as usize] {
					for plane in &layer.planes[..layer.nb_planes as usize] {
						frame.planes[frame.plane_count] =
							(plane.offset as usize, plane.pitch as usize);
						frame.plane_count += 1;
					}
				}
				(frame.modifier, frame.fd, frame.size) =
					(object.format_modifier, object.fd, object.size);
				assert!(frame.fd >= 0 && frame.size > 0);
				exported
			}
		}
	}

	impl Drop for Exported {
		fn drop(&mut self) {
			let api = &Ffmpeg::get().unwrap().api;
			// SAFETY: ours; the mapping goes before the surface it maps.
			unsafe {
				(api.av_frame_free)(&mut self.drm);
				(api.av_frame_free)(&mut self.surface);
				(api.av_buffer_unref)(&mut self.pool);
			}
		}
	}

	/// The picture of a GPU frame, read back into memory as I420.
	fn download(frame: &GpuFrame) -> VideoFrame {
		let ffmpeg = Ffmpeg::get().unwrap();
		let api = &ffmpeg.api;
		let (w, h) = (frame.width as usize, frame.height as usize);
		let mut out = VideoFrame::black_i420(frame.width, frame.height);
		let FrameData::I420 { y, u, v } = &mut out.data else { unreachable!() };
		// SAFETY: `sw` is ours and freed below; a downloaded plane holds
		// `linesize * rows` bytes.
		unsafe {
			let mut sw = (api.av_frame_alloc)();
			(*sw.cast::<FrameHead>()).format = ffmpeg.pix.nv12;
			assert!((api.av_hwframe_transfer_data)(sw, frame.surface.0, 0) >= 0, "download");
			let head = &*sw.cast::<FrameHead>();
			let rows = |i: usize, rows: usize| {
				let stride = head.linesize[i] as usize;
				std::slice::from_raw_parts(head.data[i], stride * rows).chunks_exact(stride)
			};
			for (row, src) in rows(0, h).enumerate() {
				y.data[row * w..][..w].copy_from_slice(&src[..w]);
			}
			let cw = w / 2;
			for (row, src) in rows(1, h / 2).enumerate() {
				for (i, pair) in src[..cw * 2].chunks_exact(2).enumerate() {
					u.data[row * u.stride + i] = pair[0];
					v.data[row * v.stride + i] = pair[1];
				}
			}
			(api.av_frame_free)(&mut sw);
		}
		out
	}

	/// One captured RGB DMA-BUF made into a GPU frame per simulcast layer:
	/// full size, half and a fixed 640x360 at AV1's alignment, each the
	/// whole picture scaled (cropped to the alignment) as the CPU pyramid
	/// does it, and each encoded by `h264_vaapi` straight from its surface
	/// and decoded. The frames are recycled once nothing holds them.
	#[test]
	fn gpu_frames_for_every_layer() {
		const SIZE: (u32, u32) = (1280, 720);
		if !usable("h264_vaapi") {
			eprintln!("no usable h264_vaapi, skipped");
			return;
		}
		let ffmpeg = Ffmpeg::get().unwrap();
		let screen = crate::capture::synthetic::SyntheticScreen::with_pattern(
			SIZE.0,
			SIZE.1,
			crate::capture::synthetic::Pattern::Desktop,
		);
		let picture = screen.frame(5, 30);
		let rgb =
			Exported::new(SIZE, ffmpeg.pix.bgr0.unwrap(), drm_fourcc(b"XR24"), Some(&picture));
		let mut converter = GpuConverter::new().expect("VA-API video processing");
		eprintln!("tiled RGB modifiers the import takes: {:x?}", converter.modifiers());
		let layers = [
			GpuLayer { size: SIZE, alignment: (2, 2), due: true },
			GpuLayer { size: (640, 360), alignment: (64, 16), due: true },
			GpuLayer { size: (320, 180), alignment: (2, 2), due: false },
		];
		let mut out = [None, None, None];
		converter.convert(&rgb.frame, &layers, &mut out).expect("converted");
		assert!(out[2].is_none(), "a layer that is not due gets nothing");
		let sizes: Vec<_> = out.iter().flatten().map(|f| (f.width, f.height)).collect();
		assert_eq!(sizes, [SIZE, (640, 352)], "cropped to the alignment");

		let cpu = convert::to_i420(&picture).unwrap().into_owned();
		let mut half = VideoFrame::black_i420(640, 360);
		let mut workers = crate::workers::Workers::new("voelin-test-scale", 1);
		crate::scale::scale_i420(&mut workers, &cpu, &mut half).unwrap();
		for frame in out.iter().flatten() {
			// The CPU's picture at the layer's size, cropped the same way.
			let reference = if (frame.width, frame.height) == SIZE { &cpu } else { &half };
			let mut cropped = reference.view();
			(cropped.width, cropped.height) = (frame.width, frame.height);
			let gpu = download(frame);
			let psnr = convert::psnr(&cropped.to_frame(), &gpu).unwrap();
			eprintln!("{}x{}: PSNR {psnr:.1} dB against the CPU's", frame.width, frame.height);
			assert!(psnr > 30.0, "{}x{}: {psnr:.1} dB", frame.width, frame.height);

			let mut encoder = FfmpegEncoder::new("h264_vaapi", EncoderConfig::default()).unwrap();
			let mut decoder = decoder(Codec::H264);
			let mut packets = 0;
			encoder
				.encode_gpu(frame, true, &mut |chunk| {
					packets += 1;
					if let Some(decoder) = &mut decoder {
						let decoded = decoder.decode(chunk.data).unwrap().expect("a picture");
						assert_eq!((decoded.width, decoded.height), (frame.width, frame.height));
						let psnr = convert::psnr(&gpu, &decoded).unwrap();
						assert!(psnr > 30.0, "decoded at {psnr:.1} dB");
					}
				})
				.expect("encoded from the surface");
			assert!(packets > 0);
		}
		// Once nothing holds them, the same frames come back.
		let first: Vec<*const GpuFrame> =
			out.iter().flatten().map(std::sync::Arc::as_ptr).collect();
		out = [None, None, None];
		converter.convert(&rgb.frame, &layers, &mut out).expect("converted again");
		let again: Vec<*const GpuFrame> =
			out.iter().flatten().map(std::sync::Arc::as_ptr).collect();
		assert_eq!(first, again, "recycled");
	}

	/// Our decoder for `codec`, if this build and machine have one: OpenH264
	/// with `VOELIN_OPENH264_LIB`, dav1d with `--features av1`.
	fn decoder(codec: Codec) -> Option<Box<dyn crate::codec::VideoDecoder>> {
		match codec {
			#[cfg(feature = "openh264")]
			Codec::H264 => {
				let path = std::env::var_os("VOELIN_OPENH264_LIB")?;
				let library = crate::codec::h264::OpenH264::load(path).ok()?;
				Some(Box::new(library.decoder().ok()?))
			}
			#[cfg(feature = "av1")]
			Codec::Av1 => Some(Box::new(crate::codec::av1::Dav1dDecoder::new().ok()?)),
			_ => None,
		}
	}

	/// The zero-copy path with a real DMA-BUF, on the GPU.
	///
	/// The probe reports the import as available from the offsets alone, so
	/// this makes a VA-API surface, exports it as a DMA-BUF and feeds it back
	/// to an encoder through [`FfmpegEncoder::encode_dmabuf`]: the import,
	/// the `AVFrame.hw_frames_ctx` check and the encode, for real.
	///
	/// Skipped without a working `h264_vaapi`.
	#[test]
	fn a_dmabuf_really_reaches_a_vaapi_encoder() {
		if !usable("h264_vaapi") {
			eprintln!("no usable h264_vaapi, skipped");
			return;
		}
		let nv12 = Ffmpeg::get().unwrap().pix.nv12;
		let exported = Exported::new((320, 240), nv12, drm_fourcc(b"NV12"), None);
		let frame = exported.frame;
		assert_eq!(frame.plane_count, 2, "NV12 has two planes");
		let mut encoder = FfmpegEncoder::new("h264_vaapi", EncoderConfig::default()).unwrap();
		let mut packets = 0;
		let mut keyframe = false;
		encoder
			.encode_dmabuf(&frame, true, &mut |chunk| {
				packets += 1;
				keyframe |= chunk.keyframe;
			})
			.expect("the DMA-BUF was imported and encoded");
		assert!(packets > 0 && keyframe, "{packets} packets, keyframe {keyframe}");
		eprintln!(
			"imported a {}x{} NV12 DMA-BUF (modifier {:#x}) into h264_vaapi: {packets} packet(s)",
			frame.width, frame.height, frame.modifier
		);
	}

	/// An RGB DMA-BUF, what screen capture hands over, converted to NV12 on
	/// the GPU: the conversion against the CPU's ([`convert`]), then through
	/// the encoders to our decoders. 642x362 is not a size the AV1 encoder of
	/// an RDNA3 GPU encodes exactly, so there the conversion also crops.
	///
	/// Skipped without a working `h264_vaapi`; the decoding without
	/// decoders.
	#[test]
	fn an_rgb_dmabuf_is_converted_on_the_gpu() {
		const SIZE: (u32, u32) = (642, 362);
		if !usable("h264_vaapi") {
			eprintln!("no usable h264_vaapi, skipped");
			return;
		}
		let ffmpeg = Ffmpeg::get().unwrap();
		let screen = crate::capture::synthetic::SyntheticScreen::with_pattern(
			SIZE.0,
			SIZE.1,
			crate::capture::synthetic::Pattern::Desktop,
		);
		let picture = screen.frame(3, 30);
		let rgb =
			Exported::new(SIZE, ffmpeg.pix.bgr0.unwrap(), drm_fourcc(b"XR24"), Some(&picture));
		assert_eq!(rgb.frame.plane_count, 1);

		// The conversion alone: the GPU's NV12 against the CPU's I420.
		let mut converter = GpuConverter::new().expect("VA-API video processing");
		let layer = GpuLayer { size: SIZE, alignment: (2, 2), due: true };
		let mut converted = [None];
		converter.convert(&rgb.frame, &[layer], &mut converted).expect("converted");
		let downloaded = download(converted[0].as_ref().unwrap());
		drop((converted, converter));
		let cpu = convert::to_i420(&picture).unwrap();
		let FrameData::I420 { y, u, v } = &cpu.data else { unreachable!() };
		let FrameData::I420 { y: gy, u: gu, v: gv } = &downloaded.data else { unreachable!() };
		let (w, h, cw, ch) =
			(SIZE.0 as usize, SIZE.1 as usize, SIZE.0 as usize / 2, SIZE.1 as usize / 2);
		for (name, gpu, cpu, width, rows) in
			[("Y", gy, y, w, h), ("U", gu, u, cw, ch), ("V", gv, v, cw, ch)]
		{
			let gpu: Vec<u8> = (0..rows).flat_map(|r| gpu.row(r, width).to_vec()).collect();
			let cpu: Vec<u8> = (0..rows).flat_map(|r| cpu.row(r, width).to_vec()).collect();
			let mean = gpu.iter().zip(&cpu).map(|(a, b)| u64::from(a.abs_diff(*b))).sum::<u64>()
				as f64 / cpu.len() as f64;
			eprintln!("{name}: GPU and CPU conversion differ by {mean:.2} on average");
			// Chroma differs a little at edges (another subsampling filter);
			// another matrix, range or transfer function moves whole areas.
			assert!(mean < 1.0, "{name} differs by {mean:.2}: another matrix, range or transfer?");
		}

		// The buffers: the driver's own (tiled), and a LINEAR one with a
		// padded pitch in system memory, which is what the portal negotiates.
		#[cfg(all(target_os = "linux", feature = "pipewire"))]
		let linear = linear_dmabuf(&picture, (SIZE.0 as usize * 4).next_multiple_of(256));
		#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
		let linear: Option<((), DmaBufRef)> = None;
		if linear.is_none() {
			eprintln!("no /dev/udmabuf: the LINEAR buffer is skipped");
		}
		let buffers: Vec<DmaBufRef> =
			std::iter::once(rgb.frame).chain(linear.as_ref().map(|(_, frame)| *frame)).collect();

		// Through the encoders, decoded: as good as the same picture
		// converted and uploaded by the CPU.
		for name in ["h264_vaapi", "av1_vaapi"] {
			if !usable(name) {
				continue;
			}
			// The lowest PSNR through the CPU path (`None`), then each buffer.
			let paths = std::iter::once(None).chain(buffers.iter().map(Some));
			let mut psnr = Vec::new();
			let mut coded = (0, 0);
			for buffer in paths {
				let mut encoder = FfmpegEncoder::new(name, EncoderConfig::default()).unwrap();
				let (cw, ch) = encoder.coded_size(SIZE.0, SIZE.1);
				coded = (cw, ch);
				let mut decoder = decoder(encoder.codec());
				let (mut packets, mut worst) = (0, f64::INFINITY);
				for n in 0..4u32 {
					let timestamp = Duration::from_secs(n.into()) / 30;
					let mut take = |chunk: EncodedChunk<'_>| {
						packets += 1;
						let Some(decoder) = &mut decoder else { return };
						let Some(decoded) = decoder.decode(chunk.data).unwrap() else { return };
						assert_eq!((decoded.width, decoded.height), (cw, ch), "{name}");
						let mut source = picture.view();
						(source.width, source.height) = (cw, ch);
						worst = worst.min(convert::psnr(&source.to_frame(), &decoded).unwrap());
					};
					let result = match buffer {
						Some(buffer) => {
							let frame = DmaBufRef { timestamp, ..*buffer };
							encoder.encode_dmabuf(&frame, n == 0, &mut take)
						}
						None => {
							let frame = picture.clone().with_timestamp(timestamp);
							encoder.encode_with(&frame, n == 0, &mut take)
						}
					};
					result.unwrap_or_else(|e| panic!("{name}: {e}"));
				}
				assert!(packets >= 3, "{name}: {packets} packets");
				psnr.push(worst);
			}
			let cpu = psnr[0];
			for (buffer, gpu) in buffers.iter().zip(&psnr[1..]) {
				assert!(
					gpu.is_infinite() || *gpu > cpu - 0.5,
					"{name}: {gpu:.1} dB against {cpu:.1}"
				);
				eprintln!(
					"{name}: {}x{} XR24 DMA-BUF (modifier {:#x}, pitch {}) encoded at {}x{}, lowest \
					 PSNR {gpu:.1} dB (the CPU path {cpu:.1} dB)",
					SIZE.0, SIZE.1, buffer.modifier, buffer.planes[0].1, coded.0, coded.1
				);
			}
		}
	}

	/// `picture` (BGRA) in a LINEAR DMA-BUF of ordinary memory, rows
	/// `stride` bytes apart ([`Udmabuf`](crate::capture::dmabuf::Udmabuf));
	/// the buffer has to outlive the reference. `None` without access to
	/// `/dev/udmabuf`.
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	fn linear_dmabuf(
		picture: &VideoFrame,
		stride: usize,
	) -> Option<(crate::capture::dmabuf::Udmabuf, DmaBufRef)> {
		let FrameData::Bgra(pixels) = &picture.data else { return None };
		let (w, h) = (picture.width as usize, picture.height as usize);
		let mut buffer = crate::capture::dmabuf::Udmabuf::new(stride * h).ok()?;
		for (y, row) in buffer.bytes_mut().chunks_exact_mut(stride).take(h).enumerate() {
			row[..w * 4].copy_from_slice(pixels.row(y, w * 4));
		}
		let frame = DmaBufRef {
			width: picture.width,
			height: picture.height,
			timestamp: Duration::ZERO,
			fourcc: drm_fourcc(b"XR24"),
			modifier: crate::capture::DRM_MOD_LINEAR,
			fd: buffer.fd(),
			size: buffer.len(),
			planes: [(0, stride), (0, 0), (0, 0), (0, 0)],
			plane_count: 1,
		};
		Some((buffer, frame))
	}

	/// What the CPU spends per frame on its way into a VA-API encoder at
	/// 2560x1440: the shared-memory path (the picture converted to I420 on
	/// every core, then interleaved and uploaded by `encode_with`) against
	/// an RGB DMA-BUF converted on the GPU. CPU time is every thread of the
	/// process (`/proc/self/task/*/schedstat`), so run it alone:
	/// `cargo test -p voelin-media --release --lib zero_copy_cpu_cost --
	/// --ignored --nocapture`.
	#[test]
	#[ignore = "a measurement, not a check"]
	fn zero_copy_cpu_cost() {
		const SIZE: (u32, u32) = (2560, 1440);
		const FRAMES: u32 = 600;
		if !usable("h264_vaapi") {
			eprintln!("no usable h264_vaapi, skipped");
			return;
		}
		let ffmpeg = Ffmpeg::get().unwrap();
		let cpu_time = || -> Duration {
			let tasks = std::fs::read_dir("/proc/self/task").unwrap().flatten();
			tasks
				.filter_map(|t| std::fs::read_to_string(t.path().join("schedstat")).ok())
				.filter_map(|s| s.split_whitespace().next()?.parse().ok())
				.map(Duration::from_nanos)
				.sum()
		};
		let measure = |what: &str, encode: &mut dyn FnMut(Duration)| {
			for n in 0..30 {
				encode(Duration::from_secs(n) / 60);
			}
			let (cpu, wall) = (cpu_time(), Instant::now());
			for n in 30..30 + FRAMES {
				encode(Duration::from_secs(n.into()) / 60);
			}
			let per_frame = |d: Duration| d.as_secs_f64() * 1e3 / f64::from(FRAMES);
			let (cpu, wall) = (per_frame(cpu_time() - cpu), per_frame(wall.elapsed()));
			eprintln!(
				"{what}: {cpu:.2} ms CPU and {wall:.2} ms wall per frame, {:.2} cores at 60 fps",
				cpu * 60.0 / 1e3
			);
		};
		let screen = crate::capture::synthetic::SyntheticScreen::with_pattern(
			SIZE.0,
			SIZE.1,
			crate::capture::synthetic::Pattern::Desktop,
		);
		let picture = screen.frame(0, 60);
		let config = EncoderConfig { fps: 60, bitrate_bps: 10_000_000, ..EncoderConfig::default() };

		let mut converter = convert::Converter::new(0);
		let mut i420 = VideoFrame::black_i420(SIZE.0, SIZE.1);
		let mut encoder = FfmpegEncoder::new("h264_vaapi", config.clone()).unwrap();
		measure("CPU: convert on every core, interleave, upload", &mut |timestamp| {
			let mut source = picture.view();
			source.timestamp = timestamp;
			converter.to_i420_into(&source, &mut i420).unwrap();
			encoder.encode_with(&i420, false, &mut |_| {}).unwrap();
		});
		drop(encoder);

		let rgb =
			Exported::new(SIZE, ffmpeg.pix.bgr0.unwrap(), drm_fourcc(b"XR24"), Some(&picture));
		let mut encoder = FfmpegEncoder::new("h264_vaapi", config).unwrap();
		measure("GPU: RGB DMA-BUF imported and converted", &mut |timestamp| {
			let frame = DmaBufRef { timestamp, ..rgb.frame };
			encoder.encode_dmabuf(&frame, false, &mut |_| {}).unwrap();
		});
	}

	/// On Linux an AMF encoder is not even made unless asked for, so AMF's
	/// runtime is never loaded by default; other backends are made as
	/// before.
	#[cfg(target_os = "linux")]
	#[test]
	fn amf_is_opt_in_on_linux() {
		if std::env::var_os("VOELIN_FFMPEG_AMF").is_some() {
			eprintln!("VOELIN_FFMPEG_AMF is set, skipped");
			return;
		}
		for name in ["h264_amf", "hevc_amf", "av1_amf"] {
			let Err(Error::CodecUnavailable { reason, .. }) =
				FfmpegEncoder::new(name, EncoderConfig::default())
			else {
				panic!("{name} was made");
			};
			assert!(reason.contains("VOELIN_FFMPEG_AMF=1"), "{name}: {reason}");
		}
		if Ffmpeg::get().is_ok() {
			assert!(FfmpegEncoder::new("h264_vaapi", EncoderConfig::default()).is_ok());
		}
	}

	#[test]
	fn settings_per_backend() {
		let config = EncoderConfig::default();
		let names = |spec: &str, low_power| -> Vec<&'static str> {
			settings(BackendSpec::by_name(spec).unwrap(), &config, low_power)
				.iter()
				.map(|s| s.name)
				.collect()
		};
		assert!(names("libx264", false).contains(&"tune"));
		assert!(names("h264_nvenc", false).contains(&"forced-idr"));
		assert!(!names("h264_vaapi", false).contains(&"low_power"));
		assert!(names("h264_vaapi", true).contains(&"low_power"));
		assert!(names("libaom-av1", false).contains(&"lag-in-frames"));
		let baseline = EncoderConfig { h264_profile: H264Profile::ConstrainedBaseline, ..config };
		let x264 = settings(BackendSpec::by_name("libx264").unwrap(), &baseline, false);
		let profile = x264.iter().find(|s| s.name == "profile").unwrap();
		assert_eq!(profile.values, ["baseline"]);
	}
}
