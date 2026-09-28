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
use std::time::Duration;

use super::layout::{self, HwFramesFields};
use super::sys::{self, FrameHead, PacketHead, Ptr, Rational, cstr};
use super::{Ffmpeg, take_log};
use crate::codec::hw::EncoderFactory;
use crate::codec::{
	Codec, ContentHint, EncodedChunk, EncodedFrame, EncoderBackend, EncoderConfig, H264Profile,
	VideoEncoder,
};
use crate::convert;
use crate::frame::{FrameData, VideoFrame};
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
	/// No packet came out yet: the first one starts the stream, so it is a
	/// keyframe even where the wrapper does not flag it (rav1e).
	first_packet: bool,
	timestamps: Timestamps,
	nv12: bool,
}

// SAFETY: the FFmpeg objects belong to this session alone and are used from
// one thread at a time (`&mut self`).
unsafe impl Send for Session {}

impl Drop for Session {
	fn drop(&mut self) {
		let api = &Ffmpeg::get().expect("a session exists only with FFmpeg").api;
		// SAFETY: each pointer is NULL or owned by this session.
		unsafe {
			(api.av_frame_free)(&mut self.sw);
			(api.av_frame_free)(&mut self.hw);
			(api.av_packet_free)(&mut self.packet);
			(api.avcodec_free_context)(&mut self.ctx);
			if !self.pool.is_null() {
				(api.av_buffer_unref)(&mut self.pool);
			}
		}
	}
}

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
}

impl FfmpegEncoder {
	/// An encoder of backend `name` (nothing is opened until the first
	/// frame, whose size it takes).
	pub fn new(name: &str, config: EncoderConfig) -> Result<Self> {
		let spec = BackendSpec::by_name(name).ok_or_else(|| Error::CodecUnavailable {
			codec: Codec::H264,
			reason: format!("unknown FFmpeg encoder {name}"),
		})?;
		let ffmpeg = Ffmpeg::get()
			.map_err(|e| Error::CodecUnavailable { codec: spec.codec, reason: e.to_owned() })?;
		Ok(Self { ffmpeg, spec, config, session: None, reinit: Reinit::No, low_power: false })
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

	/// The encoder size: even (4:2:0 chroma), at least 2x2.
	fn coded_size(width: u32, height: u32) -> (u32, u32) {
		((width & !1).max(2), (height & !1).max(2))
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
			first_packet: true,
			timestamps: Timestamps::new(),
			nv12: self.spec.input != Input::Yuv420p,
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
			generic("bufsize", bitrate),
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
		list.extend(settings(self.spec, &self.config, low_power));
		for setting in &list {
			self.apply(ctx, private, setting)?;
		}
		if self.spec.is_vaapi() {
			session.pool = self.vaapi_pool(width, height)?;
			let offset = self.ffmpeg.codec_hw_frames.clone().map_err(|e| self.unavailable(e))?;
			// SAFETY: `offset` is AVCodecContext.hw_frames_ctx (checked at
			// load); the context takes the new reference and frees it.
			unsafe {
				let reference = (api.av_buffer_ref)(session.pool);
				layout::write::<Ptr>(ctx, offset, reference);
			}
		}
		// SAFETY: a configured context and its codec; no options dictionary.
		let ret = unsafe { (api.avcodec_open2)(ctx, codec, std::ptr::null_mut()) };
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

	/// A VA-API surface pool (NV12) for `width` x `height`.
	fn vaapi_pool(&self, width: u32, height: u32) -> Result<Ptr> {
		let api = &self.ffmpeg.api;
		let device = vaapi_device(self.ffmpeg).map_err(|e| self.unavailable(e))?;
		let vaapi =
			self.ffmpeg.pix.vaapi.ok_or_else(|| self.unavailable("no VA-API format".into()))?;
		// SAFETY: `device` is a live device reference.
		let mut pool = unsafe { (api.av_hwframe_ctx_alloc)(device) };
		if pool.is_null() {
			return Err(self.unavailable("av_hwframe_ctx_alloc failed".into()));
		}
		// SAFETY: `pool` was just allocated on `device`.
		let fields: HwFramesFields = match unsafe { layout::hw_frames_fields(pool, device) } {
			Ok(f) => f,
			Err(e) => {
				// SAFETY: allocated above.
				unsafe { (api.av_buffer_unref)(&mut pool) };
				return Err(self.unavailable(e));
			}
		};
		// SAFETY: the fields were located and checked on this very context;
		// setting them before av_hwframe_ctx_init is what FFmpeg expects.
		let ret = unsafe {
			let ctx = (*pool.cast::<sys::BufferRefHead>()).data.cast::<std::ffi::c_void>();
			layout::write::<c_int>(ctx, fields.format, vaapi);
			layout::write::<c_int>(ctx, fields.sw_format, self.ffmpeg.pix.nv12);
			layout::write::<c_int>(ctx, fields.width, width as c_int);
			layout::write::<c_int>(ctx, fields.height, height as c_int);
			// Surfaces are allocated on demand and recycled by the pool.
			layout::write::<c_int>(ctx, fields.initial_pool_size, 0);
			(api.av_hwframe_ctx_init)(pool)
		};
		if ret < 0 {
			// SAFETY: allocated above.
			unsafe { (api.av_buffer_unref)(&mut pool) };
			return Err(self.error("VA-API surface pool", ret));
		}
		Ok(pool)
	}

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
			break;
		}
		keyframe |= self.drain(session, out)?;
		Ok(keyframe)
	}

	fn encode_i420(
		&mut self,
		frame: &VideoFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let (w, h) = Self::coded_size(frame.width, frame.height);
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
		let mut session = self.session.take().expect("opened above");
		let result = self.encode_in(&mut session, frame, force, out);
		self.session = Some(session);
		result
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
		// Frame numbers at the session's rate, strictly increasing.
		let exact = frame.timestamp.as_secs_f64() * f64::from(session.fps);
		let mut pts = exact.round() as i64;
		if let Some(last) = session.last_pts
			&& pts <= last
		{
			pts = last + 1;
		}
		session.last_pts = Some(pts);
		session.timestamps.push(pts, frame.pts_90khz());
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

	/// Encode one frame and flush the encoder (the self-test): the packets,
	/// and whether one was a keyframe.
	fn self_test(&mut self, width: u32, height: u32) -> Result<(usize, bool)> {
		let frame = test_frame(width, height);
		let mut packets = 0;
		let mut keyframe = false;
		self.encode_with(&frame, true, &mut |chunk| {
			packets += 1;
			keyframe |= chunk.keyframe;
		})?;
		let mut session = self.session.take().expect("opened by the frame");
		let flushed = self.send(&mut session, std::ptr::null_mut(), &mut |chunk| {
			packets += 1;
			keyframe |= chunk.keyframe;
		});
		drop(session);
		flushed?;
		Ok((packets, keyframe))
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
		let Some((ctx, running)) = self.session.as_ref().map(|s| (s.ctx, s.bitrate)) else {
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
}

/// Size of the self-test frame.
const TEST_SIZE: (u32, u32) = (320, 240);

fn test_backend(spec: &'static BackendSpec) -> std::result::Result<(), String> {
	let config = EncoderConfig { fps: 30, bitrate_bps: 500_000, ..EncoderConfig::default() };
	let mut encoder = FfmpegEncoder::new(spec.name, config).map_err(|e| e.to_string())?;
	match encoder.self_test(TEST_SIZE.0, TEST_SIZE.1) {
		Ok((0, _)) => Err("no packet from the test frame".into()),
		Ok((_, false)) => Err("the test frame did not come out as a keyframe".into()),
		Ok(_) => Ok(()),
		Err(Error::CodecUnavailable { reason, .. }) => Err(reason),
		Err(e) => Err(e.to_string()),
	}
}

/// Whether FFmpeg has `name` at all (cheap, before any self-test).
fn in_build(ffmpeg: &Ffmpeg, name: &str) -> bool {
	let name = cstr(name);
	// SAFETY: a C string; returns a static codec or NULL.
	!unsafe { (ffmpeg.api.avcodec_find_encoder_by_name)(name.as_ptr()) }.is_null()
}

/// Every backend with its self-test result (run once per process, the
/// backends in parallel). Empty without FFmpeg.
pub fn probe() -> &'static [BackendStatus] {
	static PROBE: OnceLock<Vec<BackendStatus>> = OnceLock::new();
	PROBE.get_or_init(|| {
		let Ok(ffmpeg) = Ffmpeg::get() else { return Vec::new() };
		let statuses: Vec<BackendStatus> = std::thread::scope(|scope| {
			let tests: Vec<_> = BACKENDS
				.iter()
				.map(|spec| {
					let present = in_build(ffmpeg, spec.name);
					let test = present.then(|| scope.spawn(move || test_backend(spec)));
					(spec, test)
				})
				.collect();
			tests
				.into_iter()
				.map(|(spec, test)| BackendStatus {
					spec,
					available: match test {
						None => Err("not in this FFmpeg build".into()),
						Some(handle) => {
							handle.join().unwrap_or_else(|_| Err("the self-test panicked".into()))
						}
					},
				})
				.collect()
		});
		for status in &statuses {
			match &status.available {
				Ok(()) => tracing::info!(backend = status.spec.name, "FFmpeg encoder available"),
				Err(e) => {
					tracing::debug!(backend = status.spec.name, "FFmpeg encoder unusable: {e}")
				}
			}
		}
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
