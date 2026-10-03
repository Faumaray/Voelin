//! Captured RGB DMA-BUFs converted and scaled into NV12 VA-API surfaces on
//! the GPU, for VA-API encoders: screen sharing without the CPU reading a
//! pixel.
//!
//! A DMA-BUF (what the ScreenCast portal hands over) is mapped onto a VA-API
//! surface ([`map_dmabuf`]), and VA-API video processing ([`super::vpp`])
//! converts it to NV12 at the size of every layer that wants a frame, into
//! surfaces from pools of our own. The buffer is free again once
//! [`GpuConverter::convert`] returns; the surfaces travel to the encoders as
//! [`GpuFrame`]s ([`VideoEncoder::encode_gpu`](crate::VideoEncoder::encode_gpu))
//! and go back to their pools when the last reference is dropped. The
//! frames are recycled, so nothing is allocated per frame once the pools
//! are warm.
#![allow(unsafe_code)]

use std::ffi::{c_int, c_uint};
use std::sync::Arc;
use std::time::Duration;

use super::encoder::{exact_size, vaapi_display, vaapi_pool};
use super::layout::{self, FrameRefs};
use super::sys::{self, FrameHead, Ptr};
use super::{Ffmpeg, vpp};
use crate::capture::{DRM_MOD_INVALID, DRM_MOD_LINEAR, DmaBufRef, drm_fourcc};
use crate::frame::GpuFrame;
use crate::{Error, Result};

/// The DRM formats of RGB buffers the conversion takes (8 bits per
/// channel, 4 bytes per pixel, one plane; what screen capture offers).
pub(crate) const RGB_FOURCCS: [u32; 4] =
	[drm_fourcc(b"XR24"), drm_fourcc(b"AR24"), drm_fourcc(b"XB24"), drm_fourcc(b"AB24")];

/// An `AVFrame` that holds a VA-API surface (or nothing yet). Freeing it
/// gives the surface back to its pool.
pub(crate) struct Surface(pub(crate) Ptr);

// SAFETY: the frame is ours. Once it holds a surface it is only read: its
// surface id, and as the source of `av_frame_ref`, which takes references
// of its own (FFmpeg's buffer references are thread-safe).
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
	fn alloc(ffmpeg: &Ffmpeg) -> Result<Self> {
		// SAFETY: an allocation; NULL is checked.
		let frame = unsafe { (ffmpeg.api.av_frame_alloc)() };
		if frame.is_null() {
			return Err(Error::Convert("out of memory".into()));
		}
		Ok(Self(frame))
	}

	/// Another frame of the same surface (NULL, which encoders refuse, if
	/// memory runs out).
	pub(crate) fn new_ref(&self) -> Self {
		let api = &Ffmpeg::get().expect("a surface exists only with FFmpeg").api;
		// SAFETY: a new frame takes a reference to ours; freed on failure.
		unsafe {
			let mut frame = (api.av_frame_alloc)();
			if !frame.is_null() && (api.av_frame_ref)(frame, self.0) < 0 {
				(api.av_frame_free)(&mut frame);
			}
			Self(frame)
		}
	}

	/// The VA-API surface id (`data[3]` of an `AV_PIX_FMT_VAAPI` frame,
	/// `hwcontext_vaapi.h`).
	pub(crate) fn id(&self) -> c_uint {
		// SAFETY: a live frame; `data` is its leading field.
		unsafe { (*self.0.cast::<FrameHead>()).data[3] as usize as c_uint }
	}
}

impl Drop for Surface {
	fn drop(&mut self) {
		if let Ok(ffmpeg) = Ffmpeg::get() {
			// SAFETY: our frame (or NULL).
			unsafe { (ffmpeg.api.av_frame_free)(&mut self.0) };
		}
	}
}

/// Map DMA-BUF `frame` onto a VA-API surface in `dst` (an unreferenced frame
/// of ours), which takes a reference to `pool`: the surface gets that frames
/// context's device. The surface reads the buffer itself, without a copy;
/// unreferencing `dst` lets the buffer go.
pub(crate) fn map_dmabuf(
	ffmpeg: &Ffmpeg,
	frame: &DmaBufRef,
	pool: Ptr,
	dst: Ptr,
	refs: FrameRefs,
) -> std::result::Result<(), String> {
	let api = &ffmpeg.api;
	let drm_prime = ffmpeg.pix.drm_prime.ok_or("no DRM PRIME pixel format")?;
	let vaapi = ffmpeg.pix.vaapi.ok_or("no VA-API pixel format")?;
	let mut descriptor =
		sys::DrmFrameDescriptor { nb_objects: 1, nb_layers: 1, ..Default::default() };
	descriptor.objects[0] =
		sys::DrmObject { fd: frame.fd, size: frame.size, format_modifier: frame.modifier };
	descriptor.layers[0].format = frame.fourcc;
	descriptor.layers[0].nb_planes = frame.plane_count as c_int;
	for (plane, &(offset, pitch)) in
		descriptor.layers[0].planes.iter_mut().zip(&frame.planes[..frame.plane_count])
	{
		*plane = sys::DrmPlane { object_index: 0, offset: offset as isize, pitch: pitch as isize };
	}
	// SAFETY: the descriptor lives in a buffer the source frame owns (buf[0],
	// checked offset), so FFmpeg can keep it while mapped; the destination
	// gets a reference to the pool at the checked hw_frames_ctx offset;
	// av_frame_free / av_frame_unref release both.
	unsafe {
		let mut buffer = (api.av_buffer_allocz)(size_of::<sys::DrmFrameDescriptor>());
		let mut src = (api.av_frame_alloc)();
		if buffer.is_null() || src.is_null() {
			(api.av_buffer_unref)(&mut buffer);
			(api.av_frame_free)(&mut src);
			return Err("out of memory".into());
		}
		let data = (*buffer.cast::<sys::BufferRefHead>()).data;
		data.cast::<sys::DrmFrameDescriptor>().write(descriptor);
		let head = src.cast::<FrameHead>();
		(*head).format = drm_prime;
		(*head).width = frame.width as c_int;
		(*head).height = frame.height as c_int;
		(*head).data[0] = data;
		layout::write::<Ptr>(src, refs.buf, buffer);
		(*dst.cast::<FrameHead>()).format = vaapi;
		layout::write::<Ptr>(dst, refs.hw_frames_ctx, (api.av_buffer_ref)(pool));
		let ret = (api.av_hwframe_map)(dst, src, sys::HWFRAME_MAP_READ);
		(api.av_frame_free)(&mut src);
		if ret < 0 {
			(api.av_frame_unref)(dst);
			return Err(format!("DMA-BUF import: {}", api.error_text(ret)));
		}
	}
	Ok(())
}

/// One output of [`GpuConverter::convert`]: a layer's size, the multiple
/// its encoder takes, and whether it wants this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuLayer {
	/// The size the whole picture is scaled to.
	pub size: (u32, u32),
	/// What the encoded width and height must be a multiple of
	/// ([`VideoEncoder::gpu_alignment`](crate::VideoEncoder::gpu_alignment)):
	/// the scaled picture is cropped to it at the right and bottom, as
	/// encoders crop frames in memory.
	pub alignment: (u32, u32),
	pub due: bool,
}

/// NV12 surfaces of one size, and the frames that hold them, reused once
/// nothing else does.
struct Output {
	size: (u32, u32),
	pool: Ptr,
	frames: Vec<Arc<GpuFrame>>,
}

impl Drop for Output {
	fn drop(&mut self) {
		// Frames still held elsewhere keep their surfaces and the pool.
		self.frames.clear();
		if let Ok(ffmpeg) = Ffmpeg::get() {
			// SAFETY: our reference.
			unsafe { (ffmpeg.api.av_buffer_unref)(&mut self.pool) };
		}
	}
}

/// See the [module docs](self). One per capture, used from one thread.
pub struct GpuConverter {
	ffmpeg: &'static Ffmpeg,
	refs: FrameRefs,
	display: vpp::Display,
	/// Video processing, and the source size it was made for.
	vpp: Option<(vpp::Converter, (u32, u32))>,
	/// The frames context DMA-BUFs of this size are mapped onto.
	import: Option<(Ptr, (u32, u32))>,
	/// Holds a mapping while it is converted.
	mapped: Surface,
	outputs: Vec<Output>,
	modifiers: Vec<u64>,
}

// SAFETY: the FFmpeg and libva objects are ours and used from one thread
// at a time (`&mut self`).
unsafe impl Send for GpuConverter {}

impl GpuConverter {
	/// A converter on the process's VA-API device, or why there is none (no
	/// FFmpeg, no VA-API device, no video processing).
	pub fn new() -> Result<Self> {
		let ffmpeg = Ffmpeg::get().map_err(|e| Error::Convert(e.into()))?;
		let refs = ffmpeg.frame_refs.clone().map_err(Error::Convert)?;
		let display = vaapi_display(ffmpeg).map_err(Error::Convert)?;
		// Fail now rather than on the first frame.
		vpp::Converter::new(display, 64, 64).map_err(Error::Convert)?;
		let mut converter = Self {
			ffmpeg,
			refs,
			display,
			vpp: None,
			import: None,
			mapped: Surface::alloc(ffmpeg)?,
			outputs: Vec::new(),
			modifiers: Vec::new(),
		};
		converter.modifiers.extend(converter.tiled_modifier());
		Ok(converter)
	}

	/// The tiled DRM format modifiers RGB buffers may have besides LINEAR:
	/// the one this GPU's driver gives RGB surfaces of its own (it imports
	/// what it makes), if that is tiled and needs a single plane (no
	/// compression metadata). Empty if there is none.
	pub fn modifiers(&self) -> &[u64] {
		&self.modifiers
	}

	fn tiled_modifier(&self) -> Option<u64> {
		let ffmpeg = self.ffmpeg;
		let api = &ffmpeg.api;
		let drm_prime = ffmpeg.pix.drm_prime?;
		// The size of a typical screen: drivers may lay out small surfaces
		// differently.
		let mut pool = vaapi_pool(ffmpeg, 1920, 1080, ffmpeg.pix.bgr0?).ok()?;
		let (surface, drm) = (Surface::alloc(ffmpeg).ok()?, Surface::alloc(ffmpeg).ok()?);
		// SAFETY: frames and pool are ours; the exported frame's data[0] is
		// the descriptor FFmpeg filled, read while the mapping lives (`drm`
		// goes before `surface`).
		let modifier = unsafe {
			let mut modifier = None;
			if (api.av_hwframe_get_buffer)(pool, surface.0, 0) >= 0 {
				(*drm.0.cast::<FrameHead>()).format = drm_prime;
				let flags = sys::HWFRAME_MAP_READ | sys::HWFRAME_MAP_DIRECT;
				if (api.av_hwframe_map)(drm.0, surface.0, flags) >= 0 {
					let d = *(*drm.0.cast::<FrameHead>()).data[0].cast::<sys::DrmFrameDescriptor>();
					let layers = &d.layers[..(d.nb_layers.max(0) as usize).min(d.layers.len())];
					let planes: c_int = layers.iter().map(|l| l.nb_planes).sum();
					let m = d.objects[0].format_modifier;
					let tiled = m != DRM_MOD_LINEAR && m != DRM_MOD_INVALID;
					modifier = (d.nb_objects == 1 && planes == 1 && tiled).then_some(m);
				}
			}
			modifier
		};
		drop(drm);
		drop(surface);
		// SAFETY: our reference.
		unsafe { (api.av_buffer_unref)(&mut pool) };
		modifier
	}

	/// Convert `frame`, an RGB DMA-BUF (`XR24`, `AR24`, `XB24` or `AB24` in
	/// one plane, LINEAR or one of [`modifiers`](Self::modifiers)), for every
	/// due layer: each gets an NV12 [`GpuFrame`] in `out` (same index), the
	/// whole picture scaled to the layer's size and cropped to its alignment.
	/// The buffer is no longer read when this returns. On an error nothing
	/// in `out` is valid for this frame.
	pub fn convert(
		&mut self,
		frame: &DmaBufRef,
		layers: &[GpuLayer],
		out: &mut [Option<Arc<GpuFrame>>],
	) -> Result<()> {
		if !RGB_FOURCCS.contains(&frame.fourcc) || frame.plane_count != 1 {
			return Err(Error::Convert("not an RGB DMA-BUF of one plane".into()));
		}
		let source = (frame.width, frame.height);
		self.outputs.retain(|o| layers.iter().any(|l| exact_size(l.size, l.alignment) == o.size));
		self.prepare(source)?;
		let pool = self.import.as_ref().expect("prepared").0;
		map_dmabuf(self.ffmpeg, frame, pool, self.mapped.0, self.refs).map_err(Error::Convert)?;
		let result = self.convert_mapped(frame, layers, out);
		// SAFETY: our frame; this lets the buffer go.
		unsafe { (self.ffmpeg.api.av_frame_unref)(self.mapped.0) };
		result
	}

	/// The import pool and the video processing for buffers of `source`'s
	/// size.
	fn prepare(&mut self, source: (u32, u32)) -> Result<()> {
		if self.import.as_ref().is_none_or(|(_, size)| *size != source) {
			if let Some((mut pool, _)) = self.import.take() {
				// SAFETY: our reference.
				unsafe { (self.ffmpeg.api.av_buffer_unref)(&mut pool) };
			}
			let bgr0 = self.ffmpeg.pix.bgr0.ok_or(Error::Convert("no bgr0 pixel format".into()))?;
			let pool = vaapi_pool(self.ffmpeg, source.0, source.1, bgr0).map_err(Error::Convert)?;
			self.import = Some((pool, source));
		}
		if self.vpp.as_ref().is_none_or(|(_, size)| *size != source) {
			self.vpp = None;
			let converter =
				vpp::Converter::new(self.display, source.0, source.1).map_err(Error::Convert)?;
			self.vpp = Some((converter, source));
		}
		Ok(())
	}

	fn convert_mapped(
		&mut self,
		frame: &DmaBufRef,
		layers: &[GpuLayer],
		out: &mut [Option<Arc<GpuFrame>>],
	) -> Result<()> {
		let input = self.mapped.id();
		for (layer, slot) in layers.iter().zip(out.iter_mut()) {
			if !layer.due {
				continue;
			}
			let exact = exact_size(layer.size, layer.alignment);
			// The part of the picture that scales to `exact`: all of it
			// scales to `size`, the rest is cropped.
			let part = |full: u32, kept: u32, of: u32| {
				(u64::from(full) * u64::from(kept) / u64::from(of.max(1))).max(1) as u32
			};
			let src = (
				part(frame.width, exact.0, layer.size.0),
				part(frame.height, exact.1, layer.size.1),
			);
			let gpu = self.output(exact, frame.timestamp)?;
			let (vpp, _) = self.vpp.as_ref().expect("prepared");
			vpp.convert(input, src, gpu.surface.id(), exact).map_err(Error::Convert)?;
			*slot = Some(gpu);
		}
		Ok(())
	}

	/// A frame with a fresh NV12 surface of `size`: one that nothing else
	/// holds any more, else a new one.
	fn output(&mut self, size: (u32, u32), timestamp: Duration) -> Result<Arc<GpuFrame>> {
		let ffmpeg = self.ffmpeg;
		let index = match self.outputs.iter().position(|o| o.size == size) {
			Some(i) => i,
			None => {
				let pool =
					vaapi_pool(ffmpeg, size.0, size.1, ffmpeg.pix.nv12).map_err(Error::Convert)?;
				self.outputs.push(Output { size, pool, frames: Vec::new() });
				self.outputs.len() - 1
			}
		};
		let output = &mut self.outputs[index];
		let free = match output.frames.iter_mut().position(|f| Arc::get_mut(f).is_some()) {
			Some(free) => free,
			None => {
				let surface = Surface::alloc(ffmpeg)?;
				output.frames.push(Arc::new(GpuFrame {
					width: size.0,
					height: size.1,
					timestamp,
					surface,
				}));
				output.frames.len() - 1
			}
		};
		let frame = Arc::get_mut(&mut output.frames[free]).expect("nothing else holds it");
		frame.timestamp = timestamp;
		let api = &ffmpeg.api;
		// SAFETY: our frame; unreferencing gives its old surface back (once
		// the encoder that referenced it is done), then it takes a new one.
		let ret = unsafe {
			(api.av_frame_unref)(frame.surface.0);
			(api.av_hwframe_get_buffer)(output.pool, frame.surface.0, 0)
		};
		if ret < 0 {
			return Err(Error::Convert(format!("VA-API surface: {}", api.error_text(ret))));
		}
		Ok(output.frames[free].clone())
	}
}

impl Drop for GpuConverter {
	fn drop(&mut self) {
		self.outputs.clear();
		if let Some((mut pool, _)) = self.import.take() {
			// SAFETY: our reference.
			unsafe { (self.ffmpeg.api.av_buffer_unref)(&mut pool) };
		}
	}
}
