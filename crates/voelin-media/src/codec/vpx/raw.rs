//! Minimal safe wrapper around the libvpx C API (`libvpx-native-sys`).
//!
//! The only `unsafe` code for VP8 / VP9. Existing safe wrappers either cover
//! encoding only or cannot change the bitrate of a running encoder, which
//! congestion control needs, so the few calls used are wrapped here. Every
//! pointer handed to libvpx comes from a checked slice or a boxed context
//! that outlives the call.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_int, c_uint, c_ulong};
use std::mem::MaybeUninit;
use std::ptr;

use vpx_sys as ffi;

pub(super) type RawResult<T> = std::result::Result<T, String>;

fn iface(vp9: bool, encoder: bool) -> *const ffi::vpx_codec_iface {
	// SAFETY: these return pointers to static interface tables (or NULL if
	// libvpx was built without that codec); no arguments.
	unsafe {
		match (vp9, encoder) {
			(false, true) => ffi::vpx_codec_vp8_cx(),
			(true, true) => ffi::vpx_codec_vp9_cx(),
			(false, false) => ffi::vpx_codec_vp8_dx(),
			(true, false) => ffi::vpx_codec_vp9_dx(),
		}
	}
}

/// Whether the linked libvpx has this codec.
pub(super) fn available(vp9: bool, encoder: bool) -> bool {
	!iface(vp9, encoder).is_null()
}

/// The libvpx version string, e.g. `v1.14.0`.
pub(super) fn version() -> String {
	// SAFETY: returns a pointer to a static NUL-terminated string.
	unsafe { CStr::from_ptr(ffi::vpx_codec_version_str()) }.to_string_lossy().into_owned()
}

fn error_text(ctx: Option<&ffi::vpx_codec_ctx_t>, err: ffi::vpx_codec_err_t) -> String {
	// SAFETY: vpx_codec_err_to_string returns a static string for any code.
	let mut text =
		unsafe { CStr::from_ptr(ffi::vpx_codec_err_to_string(err)) }.to_string_lossy().into_owned();
	if let Some(ctx) = ctx {
		// SAFETY: the context is initialised; the detail is NULL or a string
		// owned by the context, copied before the next libvpx call.
		let detail = unsafe { ffi::vpx_codec_error_detail(ctx) };
		if !detail.is_null() {
			let detail = unsafe { CStr::from_ptr(detail) }.to_string_lossy();
			text.push_str(": ");
			text.push_str(&detail);
		}
	}
	text
}

fn check(ctx: Option<&ffi::vpx_codec_ctx_t>, err: ffi::vpx_codec_err_t) -> RawResult<()> {
	if err == ffi::vpx_codec_err_t::VPX_CODEC_OK { Ok(()) } else { Err(error_text(ctx, err)) }
}

/// Borrowed I420 planes (U and V share a stride).
pub(super) struct I420<'a> {
	pub width: u32,
	pub height: u32,
	pub y: &'a [u8],
	pub u: &'a [u8],
	pub v: &'a [u8],
	pub y_stride: usize,
	pub uv_stride: usize,
}

impl I420<'_> {
	/// libvpx reads `width` x `height` luma and the rounded-up half size of
	/// chroma through the plane pointers, so the slices must cover that.
	fn check(&self) -> RawResult<()> {
		let (w, h) = (self.width as usize, self.height as usize);
		let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
		let fits = |len: usize, stride: usize, width: usize, rows: usize| {
			stride >= width && rows > 0 && len >= stride * (rows - 1) + width
		};
		if w == 0 || h == 0 || w > 16384 || h > 16384 {
			return Err(format!("unsupported size {w}x{h}"));
		}
		if !fits(self.y.len(), self.y_stride, w, h)
			|| !fits(self.u.len(), self.uv_stride, cw, ch)
			|| !fits(self.v.len(), self.uv_stride, cw, ch)
		{
			return Err("I420 planes are too small".into());
		}
		if c_int::try_from(self.y_stride).is_err() || c_int::try_from(self.uv_stride).is_err() {
			return Err("stride too large".into());
		}
		Ok(())
	}
}

pub(super) struct EncoderParams {
	pub vp9: bool,
	pub width: u32,
	pub height: u32,
	pub bitrate_kbps: u32,
	pub keyframe_interval: Option<u32>,
	pub screen: bool,
	pub threads: u32,
	/// `cpu-used`: VP8 -16 (fastest) to -4 as a fixed speed (negative), VP9
	/// 5 to 9 in realtime mode.
	pub speed: i32,
}

/// An encoded frame, borrowed from libvpx until the next call.
pub(super) struct Packet<'a> {
	pub data: &'a [u8],
	pub keyframe: bool,
	pub pts: i64,
}

/// Longest keyframe distance libvpx is given with keyframes on request only.
const NO_KEYFRAMES: u32 = 1 << 30;

/// An initialised encoder context.
pub(super) struct Encoder {
	// Boxed: libvpx keeps pointers to the context and the configuration.
	ctx: Box<ffi::vpx_codec_ctx_t>,
	cfg: Box<ffi::vpx_codec_enc_cfg_t>,
}

// SAFETY: the context is only used through `&mut self`; libvpx contexts have
// no thread affinity.
unsafe impl Send for Encoder {}

impl Encoder {
	pub fn new(p: &EncoderParams) -> RawResult<Self> {
		let iface = iface(p.vp9, true);
		if iface.is_null() {
			return Err("libvpx was built without this encoder".into());
		}
		// SAFETY: zeroed is a valid initial state for this plain C struct;
		// vpx_codec_enc_config_default fills it.
		let mut cfg: Box<ffi::vpx_codec_enc_cfg_t> =
			Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
		check(None, unsafe { ffi::vpx_codec_enc_config_default(iface, &mut *cfg, 0) })?;

		cfg.g_w = p.width;
		cfg.g_h = p.height;
		cfg.g_timebase = ffi::vpx_rational { num: 1, den: 90_000 };
		cfg.g_threads = p.threads;
		cfg.g_error_resilient = ffi::VPX_ERROR_RESILIENT_DEFAULT;
		cfg.g_lag_in_frames = 0;
		cfg.g_pass = ffi::vpx_enc_pass::VPX_RC_ONE_PASS;
		cfg.rc_end_usage = ffi::vpx_rc_mode::VPX_CBR;
		cfg.rc_target_bitrate = p.bitrate_kbps.max(1);
		cfg.rc_min_quantizer = 2;
		cfg.rc_max_quantizer = if p.vp9 { 52 } else { 56 };
		cfg.rc_undershoot_pct = if p.vp9 { 50 } else { 100 };
		cfg.rc_overshoot_pct = 15;
		cfg.rc_buf_initial_sz = 500;
		cfg.rc_buf_optimal_sz = 600;
		cfg.rc_buf_sz = 1000;
		cfg.rc_dropframe_thresh = 0;
		cfg.rc_resize_allowed = 0;
		match p.keyframe_interval {
			Some(n) => {
				cfg.kf_mode = ffi::vpx_kf_mode::VPX_KF_AUTO;
				cfg.kf_min_dist = 0;
				cfg.kf_max_dist = n.max(1);
			}
			None => {
				// Keyframes only when forced (a viewer asks for one).
				cfg.kf_mode = ffi::vpx_kf_mode::VPX_KF_DISABLED;
				cfg.kf_max_dist = NO_KEYFRAMES;
			}
		}

		// SAFETY: zeroed context as libvpx expects before init.
		let mut ctx: Box<ffi::vpx_codec_ctx_t> =
			Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
		// SAFETY: ctx and cfg are valid and boxed (stable addresses) for the
		// life of the encoder; the ABI version comes from the same headers
		// as the struct layouts.
		check(None, unsafe {
			ffi::vpx_codec_enc_init_ver(
				&mut *ctx,
				iface,
				&*cfg,
				0,
				ffi::VPX_ENCODER_ABI_VERSION as c_int,
			)
		})?;
		let mut encoder = Self { ctx, cfg };

		encoder.set_speed(p.speed)?;
		// log2 of the thread count, for splitting the frame.
		let log2_threads = 31 - p.threads.max(1).leading_zeros();
		if p.vp9 {
			encoder.control(ffi::vp8e_enc_control_id::VP9E_SET_ROW_MT, 1)?;
			// Tiles are at least 256 pixels wide.
			let max_tiles = 31 - (p.width / 256).max(1).leading_zeros();
			let tiles = log2_threads.min(max_tiles) as c_int;
			encoder.control(ffi::vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS, tiles)?;
			// Cyclic refresh: realtime adaptive quantisation.
			encoder.control(ffi::vp8e_enc_control_id::VP9E_SET_AQ_MODE, 3)?;
			encoder.control(ffi::vp8e_enc_control_id::VP9E_SET_NOISE_SENSITIVITY, 0)?;
			if p.screen {
				let screen = ffi::vp9e_tune_content::VP9E_CONTENT_SCREEN as c_int;
				encoder.control(ffi::vp8e_enc_control_id::VP9E_SET_TUNE_CONTENT, screen)?;
			}
		} else {
			// Token partitions (1, 2, 4 or 8) let decoders use threads too.
			let partitions = log2_threads.min(3) as c_int;
			encoder.control(ffi::vp8e_enc_control_id::VP8E_SET_TOKEN_PARTITIONS, partitions)?;
			encoder.control(ffi::vp8e_enc_control_id::VP8E_SET_NOISE_SENSITIVITY, 0)?;
			if p.screen {
				encoder.control(ffi::vp8e_enc_control_id::VP8E_SET_SCREEN_CONTENT_MODE, 1)?;
			}
		}
		if p.screen {
			// Skip unchanged blocks.
			encoder.control(ffi::vp8e_enc_control_id::VP8E_SET_STATIC_THRESHOLD, 1)?;
		} else {
			// Keep keyframes near 3x an average frame to avoid bursts.
			encoder.control(ffi::vp8e_enc_control_id::VP8E_SET_MAX_INTRA_BITRATE_PCT, 300)?;
		}
		Ok(encoder)
	}

	fn control(&mut self, id: ffi::vp8e_enc_control_id, value: c_int) -> RawResult<()> {
		// SAFETY: every control used here takes one int / unsigned int
		// argument (same size and promotion through varargs).
		let err = unsafe { ffi::vpx_codec_control_(&mut *self.ctx, id as c_int, value) };
		check(Some(&self.ctx), err).map_err(|e| format!("control {id:?}: {e}"))
	}

	/// Change `cpu-used` of the running encoder (see [`EncoderParams::speed`]).
	pub fn set_speed(&mut self, speed: i32) -> RawResult<()> {
		self.control(ffi::vp8e_enc_control_id::VP8E_SET_CPUUSED, speed as c_int)
	}

	/// Change the target bitrate of the running encoder.
	pub fn set_bitrate(&mut self, kbps: u32) -> RawResult<()> {
		self.cfg.rc_target_bitrate = kbps.max(1);
		// SAFETY: ctx is initialised; cfg is the boxed configuration it was
		// created with, still valid after the call.
		let err = unsafe { ffi::vpx_codec_enc_config_set(&mut *self.ctx, &*self.cfg) };
		check(Some(&self.ctx), err)
	}

	pub fn size(&self) -> (u32, u32) {
		(self.cfg.g_w, self.cfg.g_h)
	}

	/// Encode one frame (pts and duration in 1/90000 s); `out` gets the
	/// encoded frames, borrowed from libvpx.
	pub fn encode(
		&mut self,
		img: &I420<'_>,
		pts: i64,
		duration: u64,
		keyframe: bool,
		out: &mut dyn FnMut(Packet<'_>),
	) -> RawResult<()> {
		img.check()?;
		if (img.width, img.height) != self.size() {
			return Err("frame size differs from the encoder size".into());
		}
		// SAFETY: zeroed vpx_image_t is valid; vpx_img_wrap fills the derived
		// fields (chroma shifts, bps) for I420 at this size and points plane
		// 0 at the Y data. The other planes and strides are then set to the
		// checked slices. libvpx only reads through them during
		// vpx_codec_encode (it copies into its own buffers), and the slices
		// outlive the call.
		let mut image: ffi::vpx_image_t = unsafe { MaybeUninit::zeroed().assume_init() };
		let wrapped = unsafe {
			ffi::vpx_img_wrap(
				&mut image,
				ffi::vpx_img_fmt::VPX_IMG_FMT_I420,
				img.width,
				img.height,
				1,
				img.y.as_ptr().cast_mut(),
			)
		};
		if wrapped.is_null() {
			return Err("vpx_img_wrap failed".into());
		}
		image.planes[0] = img.y.as_ptr().cast_mut();
		image.planes[1] = img.u.as_ptr().cast_mut();
		image.planes[2] = img.v.as_ptr().cast_mut();
		image.planes[3] = ptr::null_mut();
		image.stride = [img.y_stride as c_int, img.uv_stride as c_int, img.uv_stride as c_int, 0];
		image.cs = ffi::vpx_color_space::VPX_CS_BT_601;
		image.range = ffi::vpx_color_range::VPX_CR_STUDIO_RANGE;

		let flags =
			if keyframe { ffi::VPX_EFLAG_FORCE_KF as ffi::vpx_enc_frame_flags_t } else { 0 };
		// SAFETY: see above; ctx is initialised and exclusively borrowed.
		let err = unsafe {
			ffi::vpx_codec_encode(
				&mut *self.ctx,
				&image,
				pts,
				duration as c_ulong,
				flags,
				ffi::VPX_DL_REALTIME as c_ulong,
			)
		};
		check(Some(&self.ctx), err)?;
		self.drain(out);
		Ok(())
	}

	fn drain(&mut self, out: &mut dyn FnMut(Packet<'_>)) {
		let mut iter: ffi::vpx_codec_iter_t = ptr::null();
		loop {
			// SAFETY: iterates the packets of the last encode call; each
			// packet and its buffer stay valid until the next libvpx call on
			// this context, which `out` cannot make (it has no access to it).
			let pkt = unsafe { ffi::vpx_codec_get_cx_data(&mut *self.ctx, &mut iter) };
			if pkt.is_null() {
				break;
			}
			let pkt = unsafe { &*pkt };
			if pkt.kind != ffi::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
				continue;
			}
			// SAFETY: `frame` is the active union member for frame packets.
			let frame = unsafe { pkt.data.frame };
			let data: &[u8] = if frame.buf.is_null() || frame.sz == 0 {
				&[]
			} else {
				unsafe { std::slice::from_raw_parts(frame.buf.cast::<u8>(), frame.sz) }
			};
			out(Packet {
				data,
				keyframe: frame.flags & ffi::VPX_FRAME_IS_KEY != 0,
				pts: frame.pts,
			});
		}
	}
}

impl Drop for Encoder {
	fn drop(&mut self) {
		// SAFETY: the context was initialised in `new` and is destroyed once.
		unsafe { ffi::vpx_codec_destroy(&mut *self.ctx) };
	}
}

/// A decoded picture, copied out of libvpx, planes without padding.
pub(super) struct Picture {
	pub width: u32,
	pub height: u32,
	pub y: Vec<u8>,
	pub u: Vec<u8>,
	pub v: Vec<u8>,
}

pub(super) struct Decoder {
	ctx: Box<ffi::vpx_codec_ctx_t>,
}

// SAFETY: as for `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
	pub fn new(vp9: bool, threads: u32) -> RawResult<Self> {
		let iface = iface(vp9, false);
		if iface.is_null() {
			return Err("libvpx was built without this decoder".into());
		}
		let cfg = ffi::vpx_codec_dec_cfg_t { threads: threads as c_uint, w: 0, h: 0 };
		// SAFETY: zeroed context before init; cfg is only read during init.
		let mut ctx: Box<ffi::vpx_codec_ctx_t> =
			Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
		check(None, unsafe {
			ffi::vpx_codec_dec_init_ver(
				&mut *ctx,
				iface,
				&cfg,
				0,
				ffi::VPX_DECODER_ABI_VERSION as c_int,
			)
		})?;
		Ok(Self { ctx })
	}

	/// Decode one frame; returns the last picture it produced.
	pub fn decode(&mut self, data: &[u8]) -> RawResult<Option<Picture>> {
		if data.is_empty() {
			return Ok(None);
		}
		let len = c_uint::try_from(data.len()).map_err(|_| "frame too large".to_owned())?;
		// SAFETY: data is a valid slice for the duration of the call.
		let err = unsafe {
			ffi::vpx_codec_decode(&mut *self.ctx, data.as_ptr(), len, ptr::null_mut(), 0)
		};
		check(Some(&self.ctx), err)?;
		let mut picture = None;
		let mut iter: ffi::vpx_codec_iter_t = ptr::null();
		loop {
			// SAFETY: the image stays valid until the next decode call; its
			// planes are copied out below.
			let img = unsafe { ffi::vpx_codec_get_frame(&mut *self.ctx, &mut iter) };
			if img.is_null() {
				break;
			}
			picture = Some(copy_image(unsafe { &*img })?);
		}
		Ok(picture)
	}
}

impl Drop for Decoder {
	fn drop(&mut self) {
		// SAFETY: initialised in `new`, destroyed once.
		unsafe { ffi::vpx_codec_destroy(&mut *self.ctx) };
	}
}

fn copy_image(img: &ffi::vpx_image_t) -> RawResult<Picture> {
	let (swap_uv, supported) = match img.fmt {
		ffi::vpx_img_fmt::VPX_IMG_FMT_I420 => (false, true),
		ffi::vpx_img_fmt::VPX_IMG_FMT_YV12 => (true, true),
		_ => (false, false),
	};
	if !supported || img.bit_depth != 8 {
		return Err(format!("unsupported decoded format {:?} ({} bit)", img.fmt, img.bit_depth));
	}
	let (w, h) = (img.d_w as usize, img.d_h as usize);
	let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
	let plane = |i: usize, width: usize, rows: usize| -> RawResult<Vec<u8>> {
		let stride = usize::try_from(img.stride[i]).map_err(|_| "negative stride".to_owned())?;
		if img.planes[i].is_null() || stride < width {
			return Err("invalid decoded plane".into());
		}
		let mut out = Vec::with_capacity(width * rows);
		for row in 0..rows {
			// SAFETY: libvpx guarantees `rows` rows of `stride` bytes
			// (at least `width` used) behind each plane pointer of a
			// decoded image of this display size.
			let src = unsafe { std::slice::from_raw_parts(img.planes[i].add(row * stride), width) };
			out.extend_from_slice(src);
		}
		Ok(out)
	};
	let y = plane(0, w, h)?;
	let (mut u, mut v) = (plane(1, cw, ch)?, plane(2, cw, ch)?);
	if swap_uv {
		std::mem::swap(&mut u, &mut v);
	}
	Ok(Picture { width: img.d_w, height: img.d_h, y, u, v })
}
