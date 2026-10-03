//! RGB to NV12 on the GPU (VA-API video processing), so that a captured
//! RGB DMA-BUF reaches a VA-API encoder without the CPU reading a pixel.
//!
//! libva is what FFmpeg's VA-API support is built on, not one of FFmpeg's
//! libraries: it is opened at runtime (`libva.so.2`, which libavutil has
//! already loaded once a VA-API device exists) and driven on FFmpeg's own
//! `VADisplay`, so FFmpeg's surfaces (`AVFrame.data[3]`) are used as they
//! are. FFmpeg does the same conversion only in libavfilter (`scale_vaapi`),
//! which would be one more library to find and a filter graph to drive for
//! what is one call here. libva 2 has kept its ABI since 2017: structs are
//! padded to a fixed size and new fields take reserved space.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use libloading::Library;

/// `VADisplay`.
pub type Display = *mut c_void;

/// `VAProfileNone`, `VAEntrypointVideoProc`, `VA_PROGRESSIVE`,
/// `VAProcPipelineParameterBufferType`.
const PROFILE_NONE: c_int = -1;
const ENTRYPOINT_VIDEO_PROC: c_int = 10;
const PROGRESSIVE: c_int = 1;
const PROC_PIPELINE_PARAMETER_BUFFER: c_int = 41;
/// `VAProcColorStandardBT601`, `VAProcColorStandardExplicit`,
/// `VA_SOURCE_RANGE_REDUCED` / `_FULL`.
const COLOR_STANDARD_BT601: c_int = 1;
const COLOR_STANDARD_EXPLICIT: c_int = 13;
const RANGE_REDUCED: u8 = 1;
const RANGE_FULL: u8 = 2;
/// ISO/IEC 23091-2 code points: SMPTE 170M (BT.601) primaries and transfer,
/// and the identity matrix (RGB).
const SMPTE170M: u8 = 6;
const MATRIX_IDENTITY: u8 = 0;

/// `VARectangle`.
#[repr(C)]
struct Rectangle {
	x: i16,
	y: i16,
	width: u16,
	height: u16,
}

/// `VAProcColorProperties`.
#[repr(C)]
#[derive(Default)]
struct ColorProperties {
	chroma_sample_location: u8,
	color_range: u8,
	colour_primaries: u8,
	transfer_characteristics: u8,
	matrix_coefficients: u8,
	reserved: [u8; 3],
}

/// `VAProcPipelineParameterBuffer` of libva 2 (224 bytes on 64-bit
/// targets).
#[repr(C)]
struct PipelineParameters {
	surface: c_uint,
	surface_region: *const Rectangle,
	surface_color_standard: c_int,
	output_region: *const Rectangle,
	output_background_color: u32,
	output_color_standard: c_int,
	pipeline_flags: u32,
	filter_flags: u32,
	filters: *mut c_uint,
	num_filters: u32,
	forward_references: *mut c_uint,
	num_forward_references: u32,
	backward_references: *mut c_uint,
	num_backward_references: u32,
	rotation_state: u32,
	blend_state: *const c_void,
	mirror_state: u32,
	additional_outputs: *mut c_uint,
	num_additional_outputs: u32,
	input_surface_flag: u32,
	output_surface_flag: u32,
	input_color_properties: ColorProperties,
	output_color_properties: ColorProperties,
	processing_mode: c_int,
	output_hdr_metadata: *mut c_void,
	#[cfg(target_pointer_width = "64")]
	reserved: [u32; 16],
	#[cfg(not(target_pointer_width = "64"))]
	reserved: [u32; 19],
}

/// The libva functions used, with their `va.h` signatures.
struct Va {
	create_config:
		unsafe extern "C" fn(Display, c_int, c_int, *mut c_void, c_int, *mut c_uint) -> c_int,
	destroy_config: unsafe extern "C" fn(Display, c_uint) -> c_int,
	create_context: unsafe extern "C" fn(
		Display,
		c_uint,
		c_int,
		c_int,
		c_int,
		*mut c_uint,
		c_int,
		*mut c_uint,
	) -> c_int,
	destroy_context: unsafe extern "C" fn(Display, c_uint) -> c_int,
	create_buffer: unsafe extern "C" fn(
		Display,
		c_uint,
		c_int,
		c_uint,
		c_uint,
		*mut c_void,
		*mut c_uint,
	) -> c_int,
	destroy_buffer: unsafe extern "C" fn(Display, c_uint) -> c_int,
	begin_picture: unsafe extern "C" fn(Display, c_uint, c_uint) -> c_int,
	render_picture: unsafe extern "C" fn(Display, c_uint, *mut c_uint, c_int) -> c_int,
	end_picture: unsafe extern "C" fn(Display, c_uint) -> c_int,
	sync_surface: unsafe extern "C" fn(Display, c_uint) -> c_int,
	error_str: unsafe extern "C" fn(c_int) -> *const c_char,
	_lib: Library,
}

impl Va {
	fn get() -> Result<&'static Va, String> {
		static VA: OnceLock<Result<Va, String>> = OnceLock::new();
		VA.get_or_init(|| {
			// SAFETY: libva has no initialisers with requirements; once a
			// VA-API device exists this is the copy libavutil loaded.
			let lib = unsafe { Library::new("libva.so.2") }.map_err(|e| format!("libva: {e}"))?;
			macro_rules! symbol {
				($name:literal) => {
					// SAFETY: looked up with its C signature from va.h; the
					// library stays loaded as long as the pointers (`_lib`).
					*unsafe { lib.get($name) }.map_err(|e| format!("libva: {e}"))?
				};
			}
			Ok(Va {
				create_config: symbol!(b"vaCreateConfig\0"),
				destroy_config: symbol!(b"vaDestroyConfig\0"),
				create_context: symbol!(b"vaCreateContext\0"),
				destroy_context: symbol!(b"vaDestroyContext\0"),
				create_buffer: symbol!(b"vaCreateBuffer\0"),
				destroy_buffer: symbol!(b"vaDestroyBuffer\0"),
				begin_picture: symbol!(b"vaBeginPicture\0"),
				render_picture: symbol!(b"vaRenderPicture\0"),
				end_picture: symbol!(b"vaEndPicture\0"),
				sync_surface: symbol!(b"vaSyncSurface\0"),
				error_str: symbol!(b"vaErrorStr\0"),
				_lib: lib,
			})
		})
		.as_ref()
		.map_err(Clone::clone)
	}

	/// `Err` with libva's text for a status other than `VA_STATUS_SUCCESS`.
	fn check(&self, what: &str, status: c_int) -> Result<(), String> {
		if status == 0 {
			return Ok(());
		}
		// SAFETY: returns a static string for any status.
		let text = unsafe { CStr::from_ptr((self.error_str)(status)) };
		Err(format!("{what}: {}", text.to_string_lossy()))
	}
}

/// A VA-API video processing context that converts RGB surfaces into NV12
/// ones, cropping and scaling on the way.
pub struct Converter {
	va: &'static Va,
	display: Display,
	config: c_uint,
	context: c_uint,
}

// SAFETY: libva serialises calls per display; the context is used from one
// thread at a time (`&self` of a session behind `&mut`).
unsafe impl Send for Converter {}

impl Converter {
	/// A context on `display` (FFmpeg's, see
	/// [`layout::device_hwctx`](super::layout::device_hwctx)) for pictures of
	/// up to `width` x `height`.
	pub fn new(display: Display, width: u32, height: u32) -> Result<Self, String> {
		let va = Va::get()?;
		let mut config = 0;
		// SAFETY: a live display; no attributes; out pointer.
		let status = unsafe {
			(va.create_config)(
				display,
				PROFILE_NONE,
				ENTRYPOINT_VIDEO_PROC,
				std::ptr::null_mut(),
				0,
				&mut config,
			)
		};
		va.check("no VA-API video processing", status)?;
		let mut context = 0;
		// SAFETY: the config just made; no render targets (surfaces are
		// named per picture); out pointer.
		let status = unsafe {
			(va.create_context)(
				display,
				config,
				width as c_int,
				height as c_int,
				PROGRESSIVE,
				std::ptr::null_mut(),
				0,
				&mut context,
			)
		};
		if let Err(e) = va.check("VA-API video processing context", status) {
			// SAFETY: made above.
			unsafe { (va.destroy_config)(display, config) };
			return Err(e);
		}
		Ok(Self { va, display, config, context })
	}

	/// Convert the top-left `src` (width, height) of RGB surface `input`
	/// into the top-left `dst` of NV12 surface `output` (both of this
	/// display), scaled if the two differ: BT.601, limited range, what the
	/// encoders signal and the CPU path ([`crate::convert`]) produces.
	///
	/// Returns once the GPU has read `input`, so a captured buffer can go
	/// back to the compositor; `output` is ready for an encoder of the same
	/// display, which orders its reads after this.
	pub fn convert(
		&self,
		input: c_uint,
		src: (u32, u32),
		output: c_uint,
		dst: (u32, u32),
	) -> Result<(), String> {
		let va = self.va;
		let rect = |(width, height): (u32, u32)| Rectangle {
			x: 0,
			y: 0,
			width: width.min(u32::from(u16::MAX)) as u16,
			height: height.min(u32::from(u16::MAX)) as u16,
		};
		let (src, dst) = (rect(src), rect(dst));
		let mut parameters = PipelineParameters {
			surface: input,
			surface_region: &src,
			output_region: &dst,
			output_background_color: 0xff00_0000,
			output_color_standard: COLOR_STANDARD_BT601,
			pipeline_flags: 0,
			filter_flags: 0,
			filters: std::ptr::null_mut(),
			num_filters: 0,
			forward_references: std::ptr::null_mut(),
			num_forward_references: 0,
			backward_references: std::ptr::null_mut(),
			num_backward_references: 0,
			rotation_state: 0,
			blend_state: std::ptr::null(),
			mirror_state: 0,
			additional_outputs: std::ptr::null_mut(),
			num_additional_outputs: 0,
			input_surface_flag: 0,
			output_surface_flag: 0,
			// The input is described with the output's primaries and transfer,
			// so the driver applies the matrix and nothing else, as the CPU
			// path does. Left to itself (no standard, or sRGB) Mesa also
			// converts the transfer function: measured on an RX 7900 GRE,
			// grey 30 came out as Y 47 instead of 42, every dark pixel about
			// 5 levels too bright.
			surface_color_standard: COLOR_STANDARD_EXPLICIT,
			input_color_properties: ColorProperties {
				color_range: RANGE_FULL,
				colour_primaries: SMPTE170M,
				transfer_characteristics: SMPTE170M,
				matrix_coefficients: MATRIX_IDENTITY,
				..Default::default()
			},
			output_color_properties: ColorProperties {
				color_range: RANGE_REDUCED,
				..Default::default()
			},
			processing_mode: 0,
			output_hdr_metadata: std::ptr::null_mut(),
			reserved: Default::default(),
		};
		let (display, context) = (self.display, self.context);
		// SAFETY: surfaces and context of this display; the parameters are
		// copied by vaCreateBuffer (and the regions they point to live until
		// vaEndPicture returns); the buffer is ours to destroy afterwards.
		unsafe {
			va.check("vaBeginPicture", (va.begin_picture)(display, context, output))?;
			let mut buffer = 0;
			let status = (va.create_buffer)(
				display,
				context,
				PROC_PIPELINE_PARAMETER_BUFFER,
				size_of::<PipelineParameters>() as c_uint,
				1,
				(&raw mut parameters).cast(),
				&mut buffer,
			);
			let rendered = va.check("vaCreateBuffer", status).and_then(|()| {
				va.check("vaRenderPicture", (va.render_picture)(display, context, &mut buffer, 1))
			});
			// A picture that was begun has to end, rendered or not.
			let ended = va.check("vaEndPicture", (va.end_picture)(display, context));
			if status == 0 {
				(va.destroy_buffer)(display, buffer);
			}
			rendered?;
			ended?;
			va.check("vaSyncSurface", (va.sync_surface)(display, output))
		}
	}
}

impl Drop for Converter {
	fn drop(&mut self) {
		// SAFETY: made in `new` on this display, no longer used.
		unsafe {
			(self.va.destroy_context)(self.display, self.context);
			(self.va.destroy_config)(self.display, self.config);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::mem::offset_of;

	use super::*;

	/// The mirror against `va/va_vpp.h` of libva 2.24 (x86-64), compiled.
	#[cfg(target_pointer_width = "64")]
	#[test]
	fn pipeline_parameters_match_the_header() {
		assert_eq!(size_of::<PipelineParameters>(), 224);
		assert_eq!(offset_of!(PipelineParameters, output_color_standard), 36);
		assert_eq!(offset_of!(PipelineParameters, rotation_state), 92);
		assert_eq!(offset_of!(PipelineParameters, input_surface_flag), 124);
		assert_eq!(offset_of!(PipelineParameters, input_color_properties), 132);
		assert_eq!(offset_of!(PipelineParameters, output_color_properties), 140);
		assert_eq!(offset_of!(PipelineParameters, processing_mode), 148);
		assert_eq!(offset_of!(ColorProperties, color_range), 1);
	}
}
