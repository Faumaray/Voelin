//! Video decoders through FFmpeg: the platform's hardware decoders first
//! (VA-API and, with NVIDIA's driver, NVDEC on Linux; D3D11VA and DXVA2 on
//! Windows; VideoToolbox on macOS), then FFmpeg's software decoders (dav1d
//! and libaom for AV1, FFmpeg's own HEVC, VP9, H.264 and VP8), each checked
//! by a self-test.
//!
//! Hardware decoding goes through FFmpeg's hwaccel API: FFmpeg's own decoder
//! (`av1`, `h264`, ...) with `AVCodecContext.hw_device_ctx` set (located
//! at load, see [`super::layout`]), so that its default `get_format` picks
//! the device's surfaces. Pictures are copied out of the GPU
//! (`av_hwframe_transfer_data`) into a frame of our own, reused, and from
//! there into the caller's [`VideoFrame`] (NV12), whose buffers are reused
//! as well ([`VideoDecoder::decode_into`]): a running decoder allocates
//! nothing per picture on our side. Software decoders give I420 (or NV12).
//!
//! Every decoder is set up for live video: slice threads only (frame
//! threads hold one frame per thread before the first picture comes out),
//! dav1d with a frame delay of one, so each frame's picture comes out of
//! the call that decoded it. VP8 decodes on one thread: its slice threads
//! split frames by token partitions and wait on each other ([`decoder_threads`]).
//!
//! [`probe`] runs every decoder once per process (in parallel) on a short
//! clip of its codec: three frames of a test picture encoded by this crate's
//! own encoders (`clips/`). Each frame must give its picture at once, at its
//! size and close to the test picture (PSNR), and a hardware decoder must
//! have decoded on the GPU: FFmpeg silently decodes in software when the
//! GPU lacks the codec (VP8 on AMD), which the self-test reports as
//! unusable, since FFmpeg's software decoder follows in the ladder anyway.
#![allow(unsafe_code)]

use std::ffi::c_int;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::Ffmpeg;
use super::encoder::{Device, log_suffix, vaapi_device};
use super::layout;
use super::sys::{EAGAIN, EOF, FrameHead, OPTION_NOT_FOUND, PacketHead, Ptr, cstr};
use crate::codec::hw::DecoderFactory;
use crate::codec::{Codec, DecoderBackend, VideoDecoder};
use crate::convert;
use crate::frame::{FrameData, FrameRef, PixelsRef, PlaneRef, VideoFrame, chroma_size};
use crate::{Error, Result};

/// One FFmpeg decoder this crate drives.
#[derive(Debug, PartialEq, Eq)]
pub struct DecoderSpec {
	/// The name in settings (`stream.decoder_backend`) and logs: FFmpeg's
	/// name of the hardware acceleration (`h264_vaapi`, `av1_nvdec`), or of
	/// the decoder in software (`hevc`, `libdav1d`).
	pub name: &'static str,
	pub codec: Codec,
	/// FFmpeg's decoder.
	pub decoder: &'static str,
	/// For hardware decoding: FFmpeg's device type (`vaapi`, `d3d11va`, ...)
	/// and the pixel format of its pictures (`vaapi`, `d3d11`, ...).
	pub hardware: Option<(&'static str, &'static str)>,
	/// The API or library, for the UI.
	pub api: &'static str,
}

macro_rules! hardware {
	($name:literal, $codec:ident, $decoder:literal, $device:literal, $format:literal, $api:literal) => {
		DecoderSpec {
			name: $name,
			codec: Codec::$codec,
			decoder: $decoder,
			hardware: Some(($device, $format)),
			api: $api,
		}
	};
}

macro_rules! software {
	($name:literal, $codec:ident, $api:literal) => {
		DecoderSpec { name: $name, codec: Codec::$codec, decoder: $name, hardware: None, api: $api }
	};
}

/// Every decoder of this platform, best first within its codec: hardware,
/// then software. The ones FFmpeg or the machine lacks fail the probe and
/// are skipped.
pub static DECODERS: &[DecoderSpec] = &[
	#[cfg(target_os = "linux")]
	hardware!("av1_vaapi", Av1, "av1", "vaapi", "vaapi", "VA-API"),
	#[cfg(target_os = "linux")]
	hardware!("hevc_vaapi", H265, "hevc", "vaapi", "vaapi", "VA-API"),
	#[cfg(target_os = "linux")]
	hardware!("vp9_vaapi", Vp9, "vp9", "vaapi", "vaapi", "VA-API"),
	#[cfg(target_os = "linux")]
	hardware!("h264_vaapi", H264, "h264", "vaapi", "vaapi", "VA-API"),
	#[cfg(target_os = "linux")]
	hardware!("vp8_vaapi", Vp8, "vp8", "vaapi", "vaapi", "VA-API"),
	// NVIDIA's driver has no VA-API of its own.
	#[cfg(target_os = "linux")]
	hardware!("av1_nvdec", Av1, "av1", "cuda", "cuda", "NVDEC"),
	#[cfg(target_os = "linux")]
	hardware!("hevc_nvdec", H265, "hevc", "cuda", "cuda", "NVDEC"),
	#[cfg(target_os = "linux")]
	hardware!("vp9_nvdec", Vp9, "vp9", "cuda", "cuda", "NVDEC"),
	#[cfg(target_os = "linux")]
	hardware!("h264_nvdec", H264, "h264", "cuda", "cuda", "NVDEC"),
	#[cfg(target_os = "linux")]
	hardware!("vp8_nvdec", Vp8, "vp8", "cuda", "cuda", "NVDEC"),
	// Every GPU vendor's; DXVA2 for systems without Direct3D 11 video.
	#[cfg(windows)]
	hardware!("av1_d3d11va", Av1, "av1", "d3d11va", "d3d11", "D3D11VA"),
	#[cfg(windows)]
	hardware!("hevc_d3d11va", H265, "hevc", "d3d11va", "d3d11", "D3D11VA"),
	#[cfg(windows)]
	hardware!("vp9_d3d11va", Vp9, "vp9", "d3d11va", "d3d11", "D3D11VA"),
	#[cfg(windows)]
	hardware!("h264_d3d11va", H264, "h264", "d3d11va", "d3d11", "D3D11VA"),
	#[cfg(windows)]
	hardware!("av1_dxva2", Av1, "av1", "dxva2", "dxva2_vld", "DXVA2"),
	#[cfg(windows)]
	hardware!("hevc_dxva2", H265, "hevc", "dxva2", "dxva2_vld", "DXVA2"),
	#[cfg(windows)]
	hardware!("vp9_dxva2", Vp9, "vp9", "dxva2", "dxva2_vld", "DXVA2"),
	#[cfg(windows)]
	hardware!("h264_dxva2", H264, "h264", "dxva2", "dxva2_vld", "DXVA2"),
	#[cfg(target_vendor = "apple")]
	hardware!("av1_videotoolbox", Av1, "av1", "videotoolbox", "videotoolbox_vld", "VideoToolbox"),
	#[cfg(target_vendor = "apple")]
	hardware!(
		"hevc_videotoolbox",
		H265,
		"hevc",
		"videotoolbox",
		"videotoolbox_vld",
		"VideoToolbox"
	),
	#[cfg(target_vendor = "apple")]
	hardware!("vp9_videotoolbox", Vp9, "vp9", "videotoolbox", "videotoolbox_vld", "VideoToolbox"),
	#[cfg(target_vendor = "apple")]
	hardware!(
		"h264_videotoolbox",
		H264,
		"h264",
		"videotoolbox",
		"videotoolbox_vld",
		"VideoToolbox"
	),
	// FFmpeg's native AV1 decoder decodes in hardware only.
	software!("libdav1d", Av1, "dav1d"),
	software!("libaom-av1", Av1, "libaom"),
	software!("hevc", H265, "FFmpeg"),
	software!("vp9", Vp9, "FFmpeg"),
	software!("h264", H264, "FFmpeg"),
	software!("vp8", Vp8, "FFmpeg"),
];

impl DecoderSpec {
	pub fn by_name(name: &str) -> Option<&'static DecoderSpec> {
		DECODERS.iter().find(|d| d.name == name)
	}

	pub fn is_hardware(&self) -> bool {
		self.hardware.is_some()
	}
}

/// The process's device of FFmpeg type `kind` (its default one), shared by
/// all decoders; VA-API's is the encoders' (`VOELIN_VAAPI_DEVICE`, or the
/// first render node).
fn hw_device(ffmpeg: &Ffmpeg, kind: &'static str) -> std::result::Result<Ptr, String> {
	if kind == "vaapi" {
		return vaapi_device(ffmpeg);
	}
	type Devices = Vec<(&'static str, std::result::Result<Device, String>)>;
	static DEVICES: Mutex<Devices> = Mutex::new(Vec::new());
	let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
	if let Some((_, device)) = devices.iter().find(|(k, _)| *k == kind) {
		return device.as_ref().map(|d| d.0).map_err(Clone::clone);
	}
	let api = &ffmpeg.api;
	let name = cstr(kind);
	// SAFETY: a C string.
	let id = unsafe { (api.av_hwdevice_find_type_by_name)(name.as_ptr()) };
	let created = if id <= 0 {
		Err(format!("FFmpeg has no {kind} support"))
	} else {
		let mut device = std::ptr::null_mut();
		// SAFETY: a valid out pointer; the type's default device, no options.
		let ret = unsafe {
			(api.av_hwdevice_ctx_create)(&mut device, id, std::ptr::null(), std::ptr::null_mut(), 0)
		};
		if ret < 0 {
			Err(format!("{kind} device: {}{}", api.error_text(ret), log_suffix(kind)))
		} else {
			Ok(Device(device))
		}
	};
	let result = created.as_ref().map(|d| d.0).map_err(Clone::clone);
	devices.push((kind, created));
	result
}

/// Threads of a software decoder: slice (or dav1d's tile and row) threads,
/// which add no delay. Except for VP8, whose slice threads work on the
/// frame's token partitions and wait on each other: a stream of 4 or 8
/// partitions (ours, from a sender with 6 cores or more) decoded 40-80 %
/// slower on 4 threads than on one, too slow for 1440p at 60 fps. dav1d
/// ([`crate::codec::dav1d_threads`]) gets a quarter of the cores.
fn decoder_threads(codec: Codec, decoder: &str) -> usize {
	let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
	match (codec, decoder) {
		(Codec::Vp8, _) => 1,
		(_, "libdav1d") => crate::codec::dav1d_threads(),
		_ => cores.min(8),
	}
}

/// A decoder of one [`DecoderSpec`]; see the [module docs](self).
pub struct FfmpegDecoder {
	ffmpeg: &'static Ffmpeg,
	spec: &'static DecoderSpec,
	ctx: Ptr,
	packet: Ptr,
	/// The decoder's pictures.
	frame: Ptr,
	/// Hardware pictures are copied here from the GPU (reused).
	download: Ptr,
	/// The pixel format of the device's pictures.
	hw_format: Option<c_int>,
	yuvj420p: Option<c_int>,
	gpu_pictures: u64,
	cpu_pictures: u64,
}

// SAFETY: the FFmpeg objects belong to this decoder alone and are used from
// one thread at a time (`&mut self`); FFmpeg's decoders have no thread
// affinity.
unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
	/// A decoder of `spec`, opened at once (on its device for hardware
	/// decoding).
	pub fn new(spec: &'static DecoderSpec) -> Result<Self> {
		let unavailable = |reason: String| Error::CodecUnavailable { codec: spec.codec, reason };
		let ffmpeg = Ffmpeg::get().map_err(|e| unavailable(e.to_owned()))?;
		let api = &ffmpeg.api;
		// Filled in one by one; Drop frees what was made.
		let mut this = Self {
			ffmpeg,
			spec,
			ctx: std::ptr::null_mut(),
			packet: std::ptr::null_mut(),
			frame: std::ptr::null_mut(),
			download: std::ptr::null_mut(),
			hw_format: None,
			yuvj420p: api.pix_fmt("yuvj420p"),
			gpu_pictures: 0,
			cpu_pictures: 0,
		};
		let name = cstr(spec.decoder);
		// SAFETY: a C string; returns a static codec or NULL.
		let codec = unsafe { (api.avcodec_find_decoder_by_name)(name.as_ptr()) };
		if codec.is_null() {
			return Err(unavailable(format!("{} is not in this FFmpeg build", spec.decoder)));
		}
		// SAFETY: allocates a context with the decoder's private options.
		this.ctx = unsafe { (api.avcodec_alloc_context3)(codec) };
		if this.ctx.is_null() {
			return Err(unavailable("avcodec_alloc_context3 failed".into()));
		}
		// SAFETY: `ctx` is a live codec context; the first child is its
		// private options object (NULL if the decoder has none).
		let private = unsafe { (api.av_opt_child_next)(this.ctx, std::ptr::null_mut()) };
		// A hardware decoder's work is on the GPU.
		let threads =
			if spec.is_hardware() { 1 } else { decoder_threads(spec.codec, spec.decoder) };
		this.set(this.ctx, "threads", &threads.to_string());
		this.set(this.ctx, "thread_type", "slice");
		if spec.decoder == "libdav1d"
			&& this.set(private, "max_frame_delay", "1") == OPTION_NOT_FOUND
		{
			// Before FFmpeg 5.0.
			this.set(private, "framethreads", "1");
		}
		if let Some((kind, format)) = spec.hardware {
			let device = hw_device(ffmpeg, kind).map_err(unavailable)?;
			let offset = ffmpeg.codec_hw_device.clone().map_err(unavailable)?;
			let format =
				api.pix_fmt(format).ok_or_else(|| unavailable(format!("no {format} format")))?;
			this.hw_format = Some(format);
			// SAFETY: `device` is a live device reference; `offset` is
			// AVCodecContext.hw_device_ctx (located and checked at load),
			// which takes the new reference and frees it with the context.
			unsafe {
				let reference = (api.av_buffer_ref)(device);
				if reference.is_null() {
					return Err(unavailable("out of memory".into()));
				}
				layout::write::<Ptr>(this.ctx, offset, reference);
			}
		}
		// SAFETY: a configured context and its codec; no options dictionary.
		let ret = unsafe { (api.avcodec_open2)(this.ctx, codec, std::ptr::null_mut()) };
		if ret < 0 {
			return Err(this.error("open", ret));
		}
		// SAFETY: allocations, checked below.
		unsafe {
			this.packet = (api.av_packet_alloc)();
			this.frame = (api.av_frame_alloc)();
			this.download = (api.av_frame_alloc)();
		}
		if this.packet.is_null() || this.frame.is_null() || this.download.is_null() {
			return Err(unavailable("out of memory".into()));
		}
		Ok(this)
	}

	pub fn spec(&self) -> &'static DecoderSpec {
		self.spec
	}

	/// Pictures decoded on the GPU so far.
	pub fn gpu_pictures(&self) -> u64 {
		self.gpu_pictures
	}

	/// Pictures decoded in software so far (by a hardware decoder: the GPU
	/// did not take the stream, and FFmpeg decoded it itself).
	pub fn cpu_pictures(&self) -> u64 {
		self.cpu_pictures
	}

	/// Set an option of `obj` (the context, or its private options) by name;
	/// one this release does not know is skipped.
	fn set(&self, obj: Ptr, name: &str, value: &str) -> c_int {
		if obj.is_null() {
			return OPTION_NOT_FOUND;
		}
		let (name, value) = (cstr(name), cstr(value));
		// SAFETY: an AVClass-enabled object and two C strings.
		unsafe { (self.ffmpeg.api.av_opt_set)(obj, name.as_ptr(), value.as_ptr(), 0) }
	}

	fn error(&self, what: &str, code: c_int) -> Error {
		self.failed(format!(
			"{what}: {}{}",
			self.ffmpeg.api.error_text(code),
			log_suffix(self.spec.decoder)
		))
	}

	fn failed(&self, message: String) -> Error {
		Error::Decoder { codec: self.spec.codec, message: format!("{} {message}", self.spec.name) }
	}

	/// Hand `data` to the decoder. The packet points at it only during the
	/// call: it is not reference-counted, so FFmpeg copies the data into a
	/// padded buffer of its own.
	fn send(&mut self, data: &[u8], size: c_int) -> c_int {
		let packet = self.packet.cast::<PacketHead>();
		// SAFETY: our packet (from av_packet_alloc, so at least as large as
		// its leading fields); emptied again before `data` goes away.
		unsafe {
			(*packet).data = data.as_ptr().cast_mut();
			(*packet).size = size;
			let ret = (self.ffmpeg.api.avcodec_send_packet)(self.ctx, self.packet);
			(*packet).data = std::ptr::null_mut();
			(*packet).size = 0;
			ret
		}
	}

	/// Take every picture that is ready; the last one ends up in `out`.
	fn receive(&mut self, out: &mut VideoFrame) -> Result<bool> {
		let mut got = false;
		loop {
			// SAFETY: a live context and our frame.
			let ret = unsafe { (self.ffmpeg.api.avcodec_receive_frame)(self.ctx, self.frame) };
			if ret == EAGAIN || ret == EOF {
				return Ok(got);
			}
			if ret < 0 {
				return Err(self.error("decoding", ret));
			}
			let copied = self.copy_picture(out);
			// SAFETY: our frame; its buffers go back to the decoder.
			unsafe { (self.ffmpeg.api.av_frame_unref)(self.frame) };
			copied?;
			got = true;
		}
	}

	/// The decoded picture into `out`: from the GPU through the download
	/// frame, or straight from FFmpeg's buffers.
	fn copy_picture(&mut self, out: &mut VideoFrame) -> Result<()> {
		let api = &self.ffmpeg.api;
		let head = self.frame.cast::<FrameHead>();
		// SAFETY: a frame the decoder just filled.
		let (format, width, height) = unsafe { ((*head).format, (*head).width, (*head).height) };
		let source = if Some(format) == self.hw_format {
			let download = self.download.cast::<FrameHead>();
			// SAFETY: the download frame is ours: its leading fields are set
			// before av_frame_get_buffer as documented, and its buffers are
			// referenced by nothing else, so they are writable.
			unsafe {
				if (*download).data[0].is_null()
					|| ((*download).width, (*download).height) != (width, height)
				{
					(api.av_frame_unref)(self.download);
					(*download).width = width;
					(*download).height = height;
					(*download).format = self.ffmpeg.pix.nv12;
					let ret = (api.av_frame_get_buffer)(self.download, 0);
					if ret < 0 {
						return Err(self.error("picture buffer", ret));
					}
				}
				let ret = (api.av_hwframe_transfer_data)(self.download, self.frame, 0);
				if ret < 0 {
					return Err(self.error("copy from the GPU", ret));
				}
			}
			self.gpu_pictures += 1;
			self.download
		} else {
			self.cpu_pictures += 1;
			self.frame
		};
		// SAFETY: `source` holds a picture until it is unreferenced, after
		// this copy.
		unsafe { self.view(source) }?.copy_to(out);
		Ok(())
	}

	/// The picture in `frame`, borrowed.
	///
	/// # Safety
	/// `frame` must hold a picture that stays as it is while the view is
	/// used.
	unsafe fn view(&self, frame: Ptr) -> Result<FrameRef<'_>> {
		// SAFETY: guaranteed by the caller.
		let head = unsafe { &*frame.cast::<FrameHead>() };
		let (Ok(width), Ok(height)) = (u32::try_from(head.width), u32::try_from(head.height))
		else {
			return Err(self.failed("gave a picture of no size".into()));
		};
		if width == 0 || height == 0 {
			return Err(self.failed("gave a picture of no size".into()));
		}
		let (w, h) = (width as usize, height as usize);
		let (cw, ch) = chroma_size(width, height);
		let plane = |i: usize, bytes: usize, rows: usize| -> Result<PlaneRef<'_>> {
			let stride = usize::try_from(head.linesize[i]).unwrap_or(0);
			if head.data[i].is_null() || stride < bytes {
				return Err(self.failed(format!("gave plane {i} with a line size of {stride}")));
			}
			// SAFETY: FFmpeg's planes hold `linesize` bytes for every row of
			// the picture but the last, which holds at least the row.
			let data =
				unsafe { std::slice::from_raw_parts(head.data[i], stride * (rows - 1) + bytes) };
			Ok(PlaneRef::new(data, stride))
		};
		let pix = self.ffmpeg.pix;
		let pixels = if head.format == pix.yuv420p || Some(head.format) == self.yuvj420p {
			PixelsRef::I420 { y: plane(0, w, h)?, u: plane(1, cw, ch)?, v: plane(2, cw, ch)? }
		} else if head.format == pix.nv12 {
			PixelsRef::Nv12 { y: plane(0, w, h)?, uv: plane(1, cw * 2, ch)? }
		} else {
			return Err(self.failed(format!(
				"gave pictures in {} (only 8-bit 4:2:0 is supported)",
				self.ffmpeg.api.pix_fmt_name(head.format)
			)));
		};
		Ok(FrameRef { width, height, timestamp: Duration::ZERO, pixels })
	}
}

impl Drop for FfmpegDecoder {
	fn drop(&mut self) {
		let api = &self.ffmpeg.api;
		// SAFETY: each pointer is NULL or owned by this decoder; the context
		// frees its device reference.
		unsafe {
			(api.av_packet_free)(&mut self.packet);
			(api.av_frame_free)(&mut self.frame);
			(api.av_frame_free)(&mut self.download);
			(api.avcodec_free_context)(&mut self.ctx);
		}
	}
}

impl VideoDecoder for FfmpegDecoder {
	fn codec(&self) -> Codec {
		self.spec.codec
	}

	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
		let mut picture = VideoFrame::black_i420(0, 0);
		Ok(self.decode_into(data, &mut picture)?.then_some(picture))
	}

	/// FFmpeg's H.264 decoder conceals what a lost frame left missing (from
	/// the previous picture), and its HEVC decoder makes up missing
	/// references; VP8, VP9 and AV1 fail on them or show garbage.
	fn conceals_errors(&self) -> bool {
		matches!(self.spec.codec, Codec::H264 | Codec::H265)
	}

	fn decode_into(&mut self, data: &[u8], out: &mut VideoFrame) -> Result<bool> {
		if data.is_empty() {
			return Ok(false);
		}
		let size =
			c_int::try_from(data.len()).map_err(|_| self.failed("frame too large".into()))?;
		let mut got = false;
		let mut ret = self.send(data, size);
		if ret == EAGAIN {
			// Pictures have to be taken out before more data goes in.
			got |= self.receive(out)?;
			ret = self.send(data, size);
		}
		if ret < 0 {
			return Err(self.error("decoding", ret));
		}
		got |= self.receive(out)?;
		Ok(got)
	}
}

/// Creates decoders of one [`DecoderSpec`] that passed its self-test.
pub struct FfmpegDecoderFactory(pub &'static DecoderSpec);

impl FfmpegDecoderFactory {
	/// Factories of the decoders that passed their self-test, in
	/// [`DECODERS`] order (hardware first).
	pub fn available() -> Vec<FfmpegDecoderFactory> {
		probe()
			.iter()
			.filter(|s| s.available.is_ok())
			.map(|s| FfmpegDecoderFactory(s.spec))
			.collect()
	}
}

impl DecoderFactory for FfmpegDecoderFactory {
	fn backend(&self) -> DecoderBackend {
		DecoderBackend::Ffmpeg(self.0.name)
	}

	fn codec(&self) -> Codec {
		self.0.codec
	}

	fn api(&self) -> &'static str {
		self.0.api
	}

	fn is_hardware(&self) -> bool {
		self.0.is_hardware()
	}

	fn create(&self) -> Result<Box<dyn VideoDecoder>> {
		Ok(Box::new(FfmpegDecoder::new(self.0)?))
	}
}

/// Size of the self-test clips: one every hardware decoder takes (NVDEC
/// wants at least 144x144 for HEVC), and a multiple of what AV1 encoders
/// pad to (64x16 on RDNA3), so that the AV1 clip declares exactly this.
pub(crate) const CLIP_SIZE: (u32, u32) = (320, 240);

/// Frames per clip: a keyframe and two that predict from it.
const CLIP_FRAMES: usize = 3;

/// How close a decoded clip picture must be to [`clip_picture`]; the clips
/// decode at well over 40 dB.
const CLIP_PSNR: f64 = 30.0;

/// The self-test clip of `codec`: [`CLIP_FRAMES`] frames of
/// [`clip_picture`] encoded by this crate's encoders (see
/// `write_self_test_clips` in the tests), each as a little-endian `u32`
/// length and the frame as a stream carries it (Annex B for H.264 and HEVC,
/// a temporal unit of OBUs for AV1).
fn clip(codec: Codec) -> &'static [u8] {
	match codec {
		Codec::Av1 => include_bytes!("clips/av1.bin"),
		Codec::H265 => include_bytes!("clips/hevc.bin"),
		Codec::Vp9 => include_bytes!("clips/vp9.bin"),
		Codec::H264 => include_bytes!("clips/h264.bin"),
		Codec::Vp8 => include_bytes!("clips/vp8.bin"),
	}
}

fn clip_frames(codec: Codec) -> impl Iterator<Item = &'static [u8]> {
	let mut rest = clip(codec);
	std::iter::from_fn(move || {
		let (length, tail) = rest.split_first_chunk::<4>()?;
		let (frame, tail) = tail.split_at_checked(u32::from_le_bytes(*length) as usize)?;
		rest = tail;
		Some(frame)
	})
}

/// Frame `n` of the self-test clips: a luma ramp, chroma ramps the other
/// way, and a bright square that moves 24 pixels per frame.
fn clip_picture(n: usize) -> VideoFrame {
	let (width, height) = CLIP_SIZE;
	let (w, h) = (width as usize, height as usize);
	let mut frame = VideoFrame::black_i420(width, height);
	let FrameData::I420 { y, u, v } = &mut frame.data else { unreachable!("black_i420") };
	let left = 32 + 24 * n;
	for row in 0..h {
		for col in 0..w {
			let square = (left..left + 64).contains(&col) && (64..128).contains(&row);
			y.data[row * w + col] = if square { 235 } else { 16 + (col * 200 / w) as u8 };
		}
	}
	let (cw, ch) = (w / 2, h / 2);
	for row in 0..ch {
		for col in 0..cw {
			u.data[row * cw + col] = 64 + (row * 128 / ch) as u8;
			v.data[row * cw + col] = 192 - (col * 128 / cw) as u8;
		}
	}
	frame.with_timestamp(Duration::from_millis(33 * n as u64))
}

/// Decode `spec`'s clip: every frame's picture at once, at its size, close
/// to the test picture; on the GPU for a hardware decoder.
fn test_decoder(spec: &'static DecoderSpec) -> std::result::Result<(), String> {
	let reason = |e: Error| match e {
		Error::CodecUnavailable { reason, .. } => reason,
		Error::Decoder { message, .. } => message,
		e => e.to_string(),
	};
	let mut decoder = FfmpegDecoder::new(spec).map_err(reason)?;
	let mut picture = VideoFrame::black_i420(0, 0);
	let mut frames = 0;
	for (n, data) in clip_frames(spec.codec).enumerate() {
		if !decoder.decode_into(data, &mut picture).map_err(reason)? {
			return Err(format!("no picture from frame {n} of the self-test clip"));
		}
		if (picture.width, picture.height) != CLIP_SIZE {
			let (w, h) = CLIP_SIZE;
			return Err(format!(
				"frame {n} decoded at {}x{} instead of {w}x{h}",
				picture.width, picture.height
			));
		}
		let psnr = convert::psnr(&clip_picture(n), &picture).map_err(|e| e.to_string())?;
		if psnr < CLIP_PSNR {
			return Err(format!("frame {n} decoded wrong (PSNR {psnr:.1} dB)"));
		}
		frames += 1;
	}
	if frames != CLIP_FRAMES {
		return Err(format!("the {} self-test clip has {frames} frames", spec.codec));
	}
	if spec.is_hardware() && decoder.cpu_pictures > 0 {
		return Err(format!(
			"the GPU does not decode {} here (FFmpeg fell back to software){}",
			spec.codec,
			log_suffix(spec.decoder)
		));
	}
	Ok(())
}

/// Why `spec` cannot work here before trying it, if that is known: not in
/// this FFmpeg build, or (NVDEC) no NVIDIA GPU, whose driver would spend
/// its initialisation failing.
fn absent(ffmpeg: &Ffmpeg, spec: &DecoderSpec) -> Option<String> {
	let api = &ffmpeg.api;
	let name = cstr(spec.decoder);
	// SAFETY: a C string; returns a static codec or NULL.
	if unsafe { (api.avcodec_find_decoder_by_name)(name.as_ptr()) }.is_null() {
		return Some(format!("{} is not in this FFmpeg build", spec.decoder));
	}
	let (kind, _) = spec.hardware?;
	let name = cstr(kind);
	// SAFETY: a C string.
	if unsafe { (api.av_hwdevice_find_type_by_name)(name.as_ptr()) } <= 0 {
		return Some(format!("FFmpeg has no {kind} support"));
	}
	#[cfg(target_os = "linux")]
	if kind == "cuda" {
		use super::encoder::{drm_vendors, vendor};
		let vendors = drm_vendors();
		if !vendors.is_empty()
			&& !vendors.contains(&vendor::NVIDIA)
			&& !std::path::Path::new("/dev/nvidiactl").exists()
		{
			return Some("no NVIDIA GPU in this machine".into());
		}
	}
	None
}

/// A decoder and the result of its self-test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderStatus {
	pub spec: &'static DecoderSpec,
	/// `Ok` if it decoded the self-test clip, else why not.
	pub available: std::result::Result<(), String>,
	/// How long its self-test took (they run in parallel).
	pub took: Duration,
}

/// Every decoder of [`DECODERS`] with its self-test result (run once per
/// process, the decoders in parallel). Empty without FFmpeg.
pub fn probe() -> &'static [DecoderStatus] {
	static PROBE: OnceLock<Vec<DecoderStatus>> = OnceLock::new();
	PROBE.get_or_init(|| {
		let Ok(ffmpeg) = Ffmpeg::get() else { return Vec::new() };
		let started = Instant::now();
		let statuses: Vec<DecoderStatus> = std::thread::scope(|scope| {
			let tests: Vec<_> = DECODERS
				.iter()
				.map(|spec| {
					let absent = absent(ffmpeg, spec);
					let test = absent.is_none().then(|| {
						scope.spawn(move || {
							let started = Instant::now();
							(test_decoder(spec), started.elapsed())
						})
					});
					(spec, absent, test)
				})
				.collect();
			tests
				.into_iter()
				.map(|(spec, absent, test)| {
					let (available, took) = match test {
						None => (Err(absent.unwrap_or_default()), Duration::ZERO),
						Some(handle) => handle.join().unwrap_or_else(|_| {
							(Err("the self-test panicked".into()), Duration::ZERO)
						}),
					};
					DecoderStatus { spec, available, took }
				})
				.collect()
		});
		for status in &statuses {
			match &status.available {
				Ok(()) => tracing::info!(
					decoder = status.spec.name,
					ms = status.took.as_millis() as u64,
					"FFmpeg decoder available"
				),
				Err(e) => tracing::debug!(
					decoder = status.spec.name,
					ms = status.took.as_millis() as u64,
					"FFmpeg decoder unusable: {e}"
				),
			}
		}
		tracing::info!(
			ms = started.elapsed().as_millis() as u64,
			tested = statuses.iter().filter(|s| !s.took.is_zero()).count(),
			"FFmpeg decoders probed"
		);
		statuses
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::codec::{Codecs, EncoderConfig};

	#[test]
	fn decoder_table() {
		for (i, spec) in DECODERS.iter().enumerate() {
			assert!(DECODERS[..i].iter().all(|d| d.name != spec.name), "{} twice", spec.name);
			assert_eq!(DecoderSpec::by_name(spec.name), Some(spec));
			// Hardware first within a codec, so the probe's order is the
			// ladder's.
			let later_hardware =
				DECODERS[i..].iter().any(|d| d.codec == spec.codec && d.is_hardware());
			assert!(spec.is_hardware() || !later_hardware, "{} before hardware", spec.name);
		}
		assert!(DecoderSpec::by_name("h264").is_some_and(|d| !d.is_hardware()));
	}

	/// The clips hold [`CLIP_FRAMES`] frames each, the first a keyframe.
	#[test]
	fn clips_are_complete() {
		for codec in Codec::ALL {
			let frames: Vec<&[u8]> = clip_frames(codec).collect();
			assert_eq!(frames.len(), CLIP_FRAMES, "{codec}");
			assert!(frames.iter().all(|f| !f.is_empty()), "{codec}");
		}
	}

	/// libvpx, always built in on the desktop, decodes the VP8 and VP9 clips
	/// to the test picture: the clips and [`clip_picture`] belong together.
	#[cfg(feature = "vpx")]
	#[test]
	fn clips_match_the_test_picture() {
		for codec in [Codec::Vp8, Codec::Vp9] {
			let mut decoder = crate::codec::vpx::VpxDecoder::new(codec).unwrap();
			for (n, data) in clip_frames(codec).enumerate() {
				let picture = decoder.decode(data).unwrap().expect("a picture per frame");
				let psnr = convert::psnr(&clip_picture(n), &picture).unwrap();
				assert!(psnr > 40.0, "{codec} frame {n}: {psnr:.1} dB");
			}
		}
	}

	/// Writes the self-test clips (`src/ffmpeg/clips/*.bin`) with the best
	/// encoder of each codec this machine has: run again only when
	/// [`clip_picture`] changes. Needs an encoder of every codec (HEVC is
	/// encoded in hardware only).
	#[test]
	#[ignore = "writes the decoders' self-test clips"]
	fn write_self_test_clips() {
		let codecs = Codecs::new();
		let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffmpeg/clips");
		for codec in Codec::ALL {
			// A high bitrate for a high PSNR, the keyframe dominating.
			let config =
				EncoderConfig { fps: 30, bitrate_bps: 2_000_000, ..EncoderConfig::default() };
			let mut encoder = codecs.new_encoder(codec, config).unwrap();
			let mut clip = Vec::new();
			let mut frames = 0;
			// Hardware encoders may hold a frame or two.
			for n in 0..CLIP_FRAMES + 8 {
				let source = clip_picture(n.min(CLIP_FRAMES - 1));
				let source = source.with_timestamp(Duration::from_millis(33 * n as u64));
				for packet in encoder.encode(&source, n == 0).unwrap() {
					if frames < CLIP_FRAMES {
						let index = (packet.pts_90khz / 2970) as usize;
						assert_eq!(index, frames, "{codec}: packets out of order");
						assert_eq!(packet.keyframe, frames == 0, "{codec} frame {frames}");
						clip.extend_from_slice(&(packet.data.len() as u32).to_le_bytes());
						clip.extend_from_slice(&packet.data);
						frames += 1;
					}
				}
			}
			assert_eq!(frames, CLIP_FRAMES, "{codec}");
			let name = match codec {
				Codec::H265 => "hevc".to_owned(),
				c => c.name().to_ascii_lowercase(),
			};
			std::fs::write(dir.join(format!("{name}.bin")), &clip).unwrap();
			eprintln!("{codec}: {} bytes from {}", clip.len(), encoder.backend());
		}
	}
}
