//! The struct fields FFmpeg offers no function or AVOption for: where they
//! are in the loaded release, found and checked at runtime.
//!
//! Everything else goes through functions and AVOptions, whose meaning does
//! not depend on the release: the codec context is configured only with
//! `av_opt_set*` (`video_size`, `pixel_format`, `time_base`, `b`, `g`, `bf`,
//! ... and the encoders' private options), frames and packets are allocated
//! by FFmpeg, and only their leading fields are read (`sys::FrameHead`,
//! `sys::PacketHead`: unchanged since FFmpeg 0.9 and 2.1). The rest:
//!
//! | Field | Why | How it is found | Check |
//! |---|---|---|---|
//! | `AVFrame.pict_type`, `AVFrame.pts` | forcing a keyframe, timestamps | right after the leading fields: `key_frame` (removed in libavutil 60), `pict_type`, `sample_aspect_ratio`, `pts` | a fresh frame must read `pict_type` 0, `sample_aspect_ratio` {0, 1}, `pts` `AV_NOPTS_VALUE` in exactly one of the two layouts |
//! | `AVCodecContext.hw_frames_ctx` | VA-API input frames | next to fields that have AVOptions, whose offsets `av_opt_find` reports: before `hw_device_ctx`, `hwaccel_flags` (libavcodec 61 and later, after `err_recognition`), or two fields before `max_pixels` (up to 60) | the neighbours' option offsets must match the layout, and the field must be NULL in a new context |
//! | `AVHWFramesContext` `initial_pool_size`, `format`, `sw_format`, `width`, `height` | creating a frame pool | after seven pointers (eight up to libavutil 58: `internal`) | `device_ctx` must equal the device and both formats must be `AV_PIX_FMT_NONE` in a new context |
//! | `AVFrame.buf[0]`, `AVFrame.hw_frames_ctx` | importing DMA-BUFs (zero-copy) | a table per libavutil major (56-61, 64-bit only; an unlisted major disables the import) | `buf[0]` of a frame from `av_frame_get_buffer` must hold that frame's `data[0]`; `hw_frames_ctx` of a frame from `av_hwframe_get_buffer` must reference the pool |
//! | `AVHWDeviceContext.hwctx` | the `VADisplay`, for the GPU colour conversion of RGB DMA-BUFs | right after `type`, after one pointer (two up to libavutil 58: `internal`) | `type` must be the device's type in exactly that layout |
//!
//! A check that fails disables only what needs the field (VA-API, or the
//! DMA-BUF import), with the reason in the probe results. The first three
//! rows are found by searching, so a new major whose layout matches keeps
//! working without a change here; the last one is a table, because there is
//! nothing to anchor those offsets to, and an unlisted major turns the
//! DMA-BUF import off rather than guess (see [`frame_refs_table`]).
#![allow(unsafe_code)]

use std::ffi::c_int;
use std::mem::{offset_of, size_of};

use super::sys::{
	Api, BufferRefHead, FrameHead, NOPTS_VALUE, PICTURE_TYPE_NONE, Ptr, Rational, cstr,
};

/// Offsets of `pict_type` and `pts` in `AVFrame`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFields {
	pub pict_type: usize,
	pub pts: usize,
}

/// Up to libavutil 59.
#[repr(C)]
struct FrameWithKeyFrame {
	head: FrameHead,
	key_frame: c_int,
	pict_type: c_int,
	sample_aspect_ratio: Rational,
	pts: i64,
}

/// libavutil 60 and later (`key_frame` removed).
#[repr(C)]
struct FrameWithoutKeyFrame {
	head: FrameHead,
	pict_type: c_int,
	sample_aspect_ratio: Rational,
	pts: i64,
}

/// # Safety
/// `base + offset` must lie inside a live object and be aligned for `T`.
pub unsafe fn read<T: Copy>(base: Ptr, offset: usize) -> T {
	// SAFETY: guaranteed by the caller.
	unsafe { base.cast::<u8>().add(offset).cast::<T>().read() }
}

/// # Safety
/// As [`read`], and the object must be ours to write.
pub unsafe fn write<T: Copy>(base: Ptr, offset: usize, value: T) {
	// SAFETY: guaranteed by the caller.
	unsafe { base.cast::<u8>().add(offset).cast::<T>().write(value) }
}

/// Find `pict_type` and `pts` in a frame fresh from `av_frame_alloc`.
pub fn frame_fields(api: &Api) -> Result<FrameFields, String> {
	// SAFETY: allocates a frame (or returns NULL).
	let mut frame = unsafe { (api.av_frame_alloc)() };
	if frame.is_null() {
		return Err("av_frame_alloc failed".into());
	}
	let matches = |pict_type: usize, sar: usize, pts: usize| {
		// SAFETY: both layouts end with `pts` at 136 bytes (64-bit) or less,
		// inside every AVFrame (416 bytes and more); the offsets come from
		// `offset_of!` of `repr(C)` structs, so they are aligned.
		unsafe {
			read::<c_int>(frame, pict_type) == PICTURE_TYPE_NONE
				&& read::<Rational>(frame, sar) == Rational { num: 0, den: 1 }
				&& read::<i64>(frame, pts) == NOPTS_VALUE
		}
	};
	let with = matches(
		offset_of!(FrameWithKeyFrame, pict_type),
		offset_of!(FrameWithKeyFrame, sample_aspect_ratio),
		offset_of!(FrameWithKeyFrame, pts),
	);
	let without = matches(
		offset_of!(FrameWithoutKeyFrame, pict_type),
		offset_of!(FrameWithoutKeyFrame, sample_aspect_ratio),
		offset_of!(FrameWithoutKeyFrame, pts),
	);
	// SAFETY: the frame came from av_frame_alloc.
	unsafe { (api.av_frame_free)(&mut frame) };
	match (with, without) {
		(true, false) => Ok(FrameFields {
			pict_type: offset_of!(FrameWithKeyFrame, pict_type),
			pts: offset_of!(FrameWithKeyFrame, pts),
		}),
		(false, true) => Ok(FrameFields {
			pict_type: offset_of!(FrameWithoutKeyFrame, pict_type),
			pts: offset_of!(FrameWithoutKeyFrame, pts),
		}),
		_ => {
			Err("unknown AVFrame layout (pict_type / pts not where any known release has them)"
				.into())
		}
	}
}

/// The offset of an AVOption's field in `obj`, if the option exists.
fn option_offset(api: &Api, obj: Ptr, name: &str) -> Option<usize> {
	let name = cstr(name);
	// SAFETY: `obj` is an AVClass-enabled object; the name is a C string.
	let option = unsafe { (api.av_opt_find)(obj, name.as_ptr(), std::ptr::null(), 0, 0) };
	if option.is_null() {
		return None;
	}
	// SAFETY: av_opt_find returns a pointer into a static option table.
	usize::try_from(unsafe { (*option).offset }).ok()
}

/// libavcodec 61 and later: `err_recognition`, `hwaccel`, `hwaccel_context`,
/// `hw_frames_ctx`, `hw_device_ctx`, `hwaccel_flags`, `extra_hw_frames`.
#[repr(C)]
struct HwFieldsNew {
	err_recognition: c_int,
	hwaccel: Ptr,
	hwaccel_context: Ptr,
	hw_frames_ctx: Ptr,
	hw_device_ctx: Ptr,
	hwaccel_flags: c_int,
	extra_hw_frames: c_int,
}

/// libavcodec 60: `hw_frames_ctx`, `trailing_padding`, `max_pixels`,
/// `hw_device_ctx`, `hwaccel_flags`.
#[repr(C)]
struct HwFields60 {
	hw_frames_ctx: Ptr,
	trailing_padding: c_int,
	max_pixels: i64,
	hw_device_ctx: Ptr,
	hwaccel_flags: c_int,
}

/// libavcodec 58 and 59: as 60 with `sub_text_format` after
/// `hw_frames_ctx`.
#[repr(C)]
struct HwFields59 {
	hw_frames_ctx: Ptr,
	sub_text_format: c_int,
	trailing_padding: c_int,
	max_pixels: i64,
	hw_device_ctx: Ptr,
	hwaccel_flags: c_int,
}

/// Where `hw_frames_ctx` is in `AVCodecContext`, from the offsets of
/// neighbouring AVOptions, checked in a new context.
pub fn codec_hw_frames(api: &Api) -> Result<usize, String> {
	// SAFETY: a generic context (no codec); freed below.
	let mut ctx = unsafe { (api.avcodec_alloc_context3)(std::ptr::null_mut()) };
	if ctx.is_null() {
		return Err("avcodec_alloc_context3 failed".into());
	}
	let result = locate_hw_frames(api, ctx);
	// SAFETY: allocated above.
	unsafe { (api.avcodec_free_context)(&mut ctx) };
	result
}

fn locate_hw_frames(api: &Api, ctx: Ptr) -> Result<usize, String> {
	let offset = |name| option_offset(api, ctx, name);
	let flags = offset("hwaccel_flags").ok_or("no hwaccel_flags option")?;
	let found = 'found: {
		// libavcodec 61+: anchored at err_recognition and extra_hw_frames.
		if let Some(err) = offset("err_detect")
			&& let Some(base) = err.checked_sub(offset_of!(HwFieldsNew, err_recognition))
			&& base + offset_of!(HwFieldsNew, hwaccel_flags) == flags
			&& offset("extra_hw_frames") == Some(base + offset_of!(HwFieldsNew, extra_hw_frames))
		{
			break 'found base + offset_of!(HwFieldsNew, hw_frames_ctx);
		}
		// Up to 60: anchored at max_pixels. `sub_text_format` (a field up to
		// 59) only matters on 32-bit targets, where the version decides.
		// SAFETY: no arguments.
		let major = unsafe { (api.avcodec_version)() } >> 16;
		if let Some(max_pixels) = offset("max_pixels") {
			let (max_off, flags_off, frames_off) = if major < 60 {
				(
					offset_of!(HwFields59, max_pixels),
					offset_of!(HwFields59, hwaccel_flags),
					offset_of!(HwFields59, hw_frames_ctx),
				)
			} else {
				(
					offset_of!(HwFields60, max_pixels),
					offset_of!(HwFields60, hwaccel_flags),
					offset_of!(HwFields60, hw_frames_ctx),
				)
			};
			if let Some(base) = max_pixels.checked_sub(max_off)
				&& base + flags_off == flags
			{
				break 'found base + frames_off;
			}
		}
		return Err(
			"unknown AVCodecContext layout (hw_frames_ctx not next to hwaccel_flags)".into()
		);
	};
	let device = found + size_of::<Ptr>();
	// SAFETY: both offsets lie before `hwaccel_flags`, an option of this
	// context, so inside it; pointer-aligned by construction.
	let (frames, device) = unsafe { (read::<Ptr>(ctx, found), read::<Ptr>(ctx, device)) };
	if !frames.is_null() || !device.is_null() {
		return Err("AVCodecContext.hw_frames_ctx check failed (not NULL in a new context)".into());
	}
	Ok(found)
}

/// Offsets in `AVHWFramesContext`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwFramesFields {
	pub initial_pool_size: usize,
	pub format: usize,
	pub sw_format: usize,
	pub width: usize,
	pub height: usize,
}

/// Up to libavutil 58.
#[repr(C)]
struct HwFramesWithInternal {
	av_class: Ptr,
	internal: Ptr,
	device_ref: Ptr,
	device_ctx: Ptr,
	hwctx: Ptr,
	free: Ptr,
	user_opaque: Ptr,
	pool: Ptr,
	initial_pool_size: c_int,
	format: c_int,
	sw_format: c_int,
	width: c_int,
	height: c_int,
}

/// libavutil 59 and later.
#[repr(C)]
struct HwFramesPublic {
	av_class: Ptr,
	device_ref: Ptr,
	device_ctx: Ptr,
	hwctx: Ptr,
	free: Ptr,
	user_opaque: Ptr,
	pool: Ptr,
	initial_pool_size: c_int,
	format: c_int,
	sw_format: c_int,
	width: c_int,
	height: c_int,
}

macro_rules! hw_frames_fields {
	($ty:ty) => {
		HwFramesFields {
			initial_pool_size: offset_of!($ty, initial_pool_size),
			format: offset_of!($ty, format),
			sw_format: offset_of!($ty, sw_format),
			width: offset_of!($ty, width),
			height: offset_of!($ty, height),
		}
	};
}

/// The fields of the frames context `frames` (fresh from
/// `av_hwframe_ctx_alloc(device)`; both are `AVBufferRef`s).
///
/// # Safety
/// `frames` and `device` must be live buffer references of an
/// `AVHWFramesContext` just allocated on the `AVHWDeviceContext` `device`.
pub unsafe fn hw_frames_fields(frames: Ptr, device: Ptr) -> Result<HwFramesFields, String> {
	// SAFETY: guaranteed by the caller; `data` is the leading field pair.
	let (ctx, device_ctx) = unsafe {
		(
			(*frames.cast::<BufferRefHead>()).data.cast::<std::ffi::c_void>(),
			(*device.cast::<BufferRefHead>()).data.cast::<std::ffi::c_void>(),
		)
	};
	let check = |fields: HwFramesFields, device_at: usize| {
		// SAFETY: both layouts lie within the smaller one (the current
		// public struct); offsets from `offset_of!` are aligned.
		unsafe {
			read::<Ptr>(ctx, device_at) == device_ctx
				&& read::<c_int>(ctx, fields.format) == -1
				&& read::<c_int>(ctx, fields.sw_format) == -1
		}
	};
	let old = hw_frames_fields!(HwFramesWithInternal);
	let new = hw_frames_fields!(HwFramesPublic);
	match (
		check(old, offset_of!(HwFramesWithInternal, device_ctx)),
		check(new, offset_of!(HwFramesPublic, device_ctx)),
	) {
		(true, false) => Ok(old),
		(false, true) => Ok(new),
		_ => Err("unknown AVHWFramesContext layout".into()),
	}
}

/// `AVHWDeviceContext` up to libavutil 58.
#[repr(C)]
struct DeviceWithInternal {
	av_class: Ptr,
	internal: Ptr,
	kind: c_int,
	hwctx: Ptr,
}

/// `AVHWDeviceContext` from libavutil 59 (`internal` removed).
#[repr(C)]
struct DevicePublic {
	av_class: Ptr,
	kind: c_int,
	hwctx: Ptr,
}

/// `AVHWDeviceContext.hwctx` of `device`, a device of type `kind`: the
/// API's own context (`AVVAAPIDeviceContext` for VA-API, whose first field
/// is the `VADisplay`).
///
/// Found by `type`, which comes right before it: in the old layout the
/// same offset holds the low half of the `internal` pointer, which is
/// aligned and so never a small device type.
///
/// # Safety
/// `device` must be a live buffer reference of an `AVHWDeviceContext`.
pub unsafe fn device_hwctx(device: Ptr, kind: c_int) -> Result<Ptr, String> {
	// SAFETY: guaranteed by the caller; `data` is the leading field pair,
	// and both layouts lie within the smaller one (the current struct).
	unsafe {
		let ctx = (*device.cast::<BufferRefHead>()).data.cast::<std::ffi::c_void>();
		for (kind_at, hwctx_at) in [
			(offset_of!(DevicePublic, kind), offset_of!(DevicePublic, hwctx)),
			(offset_of!(DeviceWithInternal, kind), offset_of!(DeviceWithInternal, hwctx)),
		] {
			if read::<c_int>(ctx, kind_at) == kind {
				let hwctx: Ptr = read(ctx, hwctx_at);
				return if hwctx.is_null() { Err("no device context".into()) } else { Ok(hwctx) };
			}
		}
	}
	Err("unknown AVHWDeviceContext layout".into())
}

/// Offsets of `buf[0]` and `hw_frames_ctx` in `AVFrame`, for importing
/// DMA-BUFs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRefs {
	pub buf: usize,
	pub hw_frames_ctx: usize,
}

/// The table of [`FrameRefs`] per libavutil major (64-bit targets).
///
/// Every entry is `offsetof` compiled from that release's own
/// `libavutil/frame.h`; libavutil 61's was measured against the headers
/// installed next to the library it describes (FFmpeg 9.0.1: `buf` 184,
/// `hw_frames_ctx` 328, `sizeof(AVFrame)` 424).
///
/// A major that is not listed returns `None`, which disables the DMA-BUF
/// import (and nothing else) with the reason in the probe results. It
/// deliberately does not fall back to the newest entry: `AVFrame` has both
/// grown and shrunk between majors (424 bytes in 61 against 536 in 56), so
/// an offset guessed for an unknown layout could read past the end of the
/// struct. Adding a major here needs its `offsetof` values, not a guess.
fn frame_refs_table(avutil_major: u32) -> Option<FrameRefs> {
	if size_of::<Ptr>() != 8 {
		return None;
	}
	let (buf, hw_frames_ctx) = match avutil_major {
		56 => (288, 480),
		57 | 58 => (224, 392),
		59 => (200, 352),
		60 | 61 => (184, 328),
		_ => return None,
	};
	Some(FrameRefs { buf, hw_frames_ctx })
}

/// [`FrameRefs`] of this release, with `buf[0]` checked on a real frame
/// (`hw_frames_ctx` is checked once a hardware frame exists, see
/// [`check_hw_frames_ctx`]).
pub fn frame_refs(api: &Api, yuv420p: c_int) -> Result<FrameRefs, String> {
	// SAFETY: no arguments.
	let major = unsafe { (api.avutil_version)() } >> 16;
	let refs = frame_refs_table(major).ok_or_else(|| {
		format!(
			"AVFrame.buf / hw_frames_ctx offsets are not recorded for libavutil {major} (see \
			 frame_refs_table); the import stays off until they are measured"
		)
	})?;
	// SAFETY: a frame of 16x16 yuv420p pixels; the head fields are set
	// before av_frame_get_buffer as its documentation asks; the offsets are
	// inside AVFrame for this major (the table), pointer-aligned.
	unsafe {
		let mut frame = (api.av_frame_alloc)();
		if frame.is_null() {
			return Err("av_frame_alloc failed".into());
		}
		let head = frame.cast::<FrameHead>();
		(*head).width = 16;
		(*head).height = 16;
		(*head).format = yuv420p;
		let ok = (api.av_frame_get_buffer)(frame, 0) >= 0 && {
			let buf: Ptr = read(frame, refs.buf);
			let hw: Ptr = read(frame, refs.hw_frames_ctx);
			!buf.is_null() && (*buf.cast::<BufferRefHead>()).data == (*head).data[0] && hw.is_null()
		};
		(api.av_frame_free)(&mut frame);
		if ok { Ok(refs) } else { Err(format!("AVFrame.buf check failed (libavutil {major})")) }
	}
}

/// Whether `frame` (from `av_hwframe_get_buffer(pool, ...)`) references
/// `pool` at `refs.hw_frames_ctx`.
///
/// # Safety
/// `frame` must be a live AVFrame of this release, `pool` a live buffer
/// reference.
pub unsafe fn check_hw_frames_ctx(frame: Ptr, pool: Ptr, refs: FrameRefs) -> bool {
	// SAFETY: guaranteed by the caller; the offset is inside AVFrame.
	unsafe {
		let hw: Ptr = read(frame, refs.hw_frames_ctx);
		!hw.is_null() && (*hw.cast::<BufferRefHead>()).data == (*pool.cast::<BufferRefHead>()).data
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The mirrors give the offsets compiled from FFmpeg's headers
	/// (x86-64): 4.4-7.1 `pict_type` 124 / `pts` 136, 8.0 120 / 136;
	/// `hw_frames_ctx` 16 bytes before `max_pixels` (4.4-6.1) and 16 before
	/// `hwaccel_flags` (7.1, 8.0).
	#[cfg(target_pointer_width = "64")]
	#[test]
	fn mirrors_match_the_headers() {
		assert_eq!(offset_of!(FrameWithKeyFrame, pict_type), 124);
		assert_eq!(offset_of!(FrameWithKeyFrame, pts), 136);
		assert_eq!(offset_of!(FrameWithoutKeyFrame, pict_type), 120);
		assert_eq!(offset_of!(FrameWithoutKeyFrame, pts), 136);
		assert_eq!(offset_of!(HwFields60, max_pixels) - offset_of!(HwFields60, hw_frames_ctx), 16);
		assert_eq!(offset_of!(HwFields59, max_pixels) - offset_of!(HwFields59, hw_frames_ctx), 16);
		assert_eq!(offset_of!(HwFields60, hwaccel_flags) - offset_of!(HwFields60, max_pixels), 16);
		// 7.1: err_recognition 528, hw_frames_ctx 552, hwaccel_flags 568.
		assert_eq!(offset_of!(HwFieldsNew, hw_frames_ctx), 552 - 528);
		assert_eq!(offset_of!(HwFieldsNew, hwaccel_flags), 568 - 528);
		assert_eq!(offset_of!(HwFramesWithInternal, format), 68);
		assert_eq!(offset_of!(HwFramesPublic, format), 60);
		assert_eq!(frame_refs_table(58), Some(FrameRefs { buf: 224, hw_frames_ctx: 392 }));
		assert_eq!(frame_refs_table(55), None);
	}

	/// libavutil 61 (FFmpeg 9), compiled from the installed headers on
	/// x86-64: `AVFrame` `pict_type` 120, `pts` 136, `buf` 184,
	/// `hw_frames_ctx` 328, `sizeof` 424; `AVCodecContext`
	/// `err_recognition` 528, `hw_frames_ctx` 552, `hw_device_ctx` 560,
	/// `hwaccel_flags` 568, `extra_hw_frames` 572; `AVHWFramesContext`
	/// `pool` 48, `initial_pool_size` 56, `format` 60, `sw_format` 64,
	/// `width` 68, `height` 72; `AVHWDeviceContext` `type` 8, `hwctx` 16.
	#[cfg(target_pointer_width = "64")]
	#[test]
	fn libavutil_61_matches_the_headers() {
		assert_eq!(frame_refs_table(61), Some(FrameRefs { buf: 184, hw_frames_ctx: 328 }));
		assert_eq!(offset_of!(FrameWithoutKeyFrame, pict_type), 120);
		assert_eq!(offset_of!(FrameWithoutKeyFrame, pts), 136);
		let base = 528;
		assert_eq!(base + offset_of!(HwFieldsNew, hw_frames_ctx), 552);
		assert_eq!(base + offset_of!(HwFieldsNew, hw_device_ctx), 560);
		assert_eq!(base + offset_of!(HwFieldsNew, hwaccel_flags), 568);
		assert_eq!(base + offset_of!(HwFieldsNew, extra_hw_frames), 572);
		assert_eq!(offset_of!(HwFramesPublic, pool), 48);
		assert_eq!(offset_of!(HwFramesPublic, initial_pool_size), 56);
		assert_eq!(offset_of!(HwFramesPublic, sw_format), 64);
		assert_eq!(offset_of!(HwFramesPublic, height), 72);
		assert_eq!(offset_of!(DevicePublic, kind), 8);
		assert_eq!(offset_of!(DevicePublic, hwctx), 16);
		// A major nobody measured must disable the import, not reuse 61's
		// offsets: AVFrame has shrunk between majors (424 bytes in 61
		// against 536 in 56), so reading at a guessed offset could leave
		// the struct.
		assert_eq!(frame_refs_table(62), None);
	}
}
