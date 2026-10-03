//! The studio's Opus as AAC, through FFmpeg's own codecs: RTMP services
//! take AAC, and few take anything else.
//!
//! [`AacEncoder`] decodes the Opus packets with FFmpeg's `opus` decoder and
//! encodes AAC-LC with its native `aac` encoder (both in every FFmpeg build),
//! 48 kHz stereo, configured through AVOptions like the video encoders.
//!
//! The AAC encoder takes frames of exactly 1024 samples; Opus frames have
//! 960, so the decoded samples are queued and cut into frames of our own.
//! Such a frame needs its channel layout, whose field moved between
//! releases (`AVFrame.ch_layout` since libavutil 57, `channels` and
//! `channel_layout` before), and `avcodec_send_frame` copies frames with
//! `av_frame_ref`, which refuses one without a layout unless its buffers are
//! reference-counted. So the input frame is a reference to the first frame
//! the decoder made, which FFmpeg filled in (layout, rate, format, the
//! buffer references), and only its leading fields are changed: the data
//! pointers aim at our planes, `nb_samples` is 1024 (`sys::FrameHead`,
//! unchanged since libavutil 51), plus `pts`, located at load. The
//! decoded buffers it still references are never read.
#![allow(unsafe_code)]

use std::ffi::c_int;

use super::Ffmpeg;
use super::layout::write;
use super::sys::{
	EAGAIN, EOF, FrameHead, NOPTS_VALUE, OPTION_NOT_FOUND, PacketHead, Ptr, Rational, cstr,
};

/// Samples per AAC frame (AAC-LC).
pub const FRAME: usize = 1024;
/// The sample rate of the studio's audio and of the AAC stream.
pub const RATE: u32 = 48_000;
/// The `AudioSpecificConfig` of the stream: AAC-LC (object type 2), 48 kHz
/// (sampling index 3), two channels. What an RTMP AAC sequence header
/// carries.
pub const AUDIO_SPECIFIC_CONFIG: [u8; 2] = [0x11, 0x90];

/// Opus packets in, AAC frames out; see the [module docs](self).
pub struct AacEncoder {
	ffmpeg: &'static Ffmpeg,
	decoder: Ptr,
	encoder: Ptr,
	/// Where the decoder puts its frames.
	decoded: Ptr,
	/// The encoder's input frame, made from the first decoded frame.
	input: Ptr,
	packet_in: Ptr,
	packet_out: Ptr,
	fltp: c_int,
	/// Decoded samples not encoded yet, per channel.
	pending: [Vec<f32>; 2],
	/// The time (48 kHz samples) of the first pending sample.
	pending_time: i64,
	/// The planes the input frame points at.
	planes: [Vec<f32>; 2],
}

// SAFETY: the FFmpeg objects belong to this value and are only used
// through `&mut self`; FFmpeg's codecs have no thread affinity.
unsafe impl Send for AacEncoder {}

impl AacEncoder {
	/// An encoder at `bitrate` bit/s.
	pub fn new(bitrate: u64) -> Result<Self, String> {
		let ffmpeg = Ffmpeg::get()?;
		let api = &ffmpeg.api;
		let name = cstr("fltp");
		// SAFETY: a C string.
		let fltp = unsafe { (api.av_get_sample_fmt)(name.as_ptr()) };
		if fltp < 0 {
			return Err("FFmpeg has no planar float samples".into());
		}
		// The fields are filled one by one; Drop frees what was made.
		let mut this = Self {
			ffmpeg,
			decoder: std::ptr::null_mut(),
			encoder: std::ptr::null_mut(),
			decoded: std::ptr::null_mut(),
			input: std::ptr::null_mut(),
			packet_in: std::ptr::null_mut(),
			packet_out: std::ptr::null_mut(),
			fltp,
			pending: [Vec::with_capacity(4 * FRAME), Vec::with_capacity(4 * FRAME)],
			pending_time: 0,
			planes: [vec![0.0; FRAME], vec![0.0; FRAME]],
		};
		// SAFETY: allocations whose results are checked; contexts of the
		// codecs they are opened with; options set by name on a codec
		// context (an AVClass object) with C strings, and `sample_fmt`
		// written at the offset located and checked at load.
		unsafe {
			this.decoded = (api.av_frame_alloc)();
			this.packet_in = (api.av_packet_alloc)();
			this.packet_out = (api.av_packet_alloc)();
			if this.decoded.is_null() || this.packet_in.is_null() || this.packet_out.is_null() {
				return Err("out of memory".into());
			}
			let opus = cstr("opus");
			let codec = (api.avcodec_find_decoder_by_name)(opus.as_ptr());
			if codec.is_null() {
				return Err("this FFmpeg has no Opus decoder".into());
			}
			this.decoder = (api.avcodec_alloc_context3)(codec);
			if this.decoder.is_null() {
				return Err("avcodec_alloc_context3 failed".into());
			}
			// Without extradata the decoder gives stereo at 48 kHz.
			let ret = (api.avcodec_open2)(this.decoder, codec, std::ptr::null_mut());
			if ret < 0 {
				return Err(format!("cannot open the Opus decoder: {}", api.error_text(ret)));
			}
			let aac = cstr("aac");
			let codec = (api.avcodec_find_encoder_by_name)(aac.as_ptr());
			if codec.is_null() {
				return Err("this FFmpeg has no AAC encoder".into());
			}
			this.encoder = (api.avcodec_alloc_context3)(codec);
			if this.encoder.is_null() {
				return Err("avcodec_alloc_context3 failed".into());
			}
			let ctx = this.encoder;
			let set = |name: &str, value: &str| {
				let (name, value) = (cstr(name), cstr(value));
				(api.av_opt_set)(ctx, name.as_ptr(), value.as_ptr(), 0)
			};
			let set_int = |name: &str, value: i64| {
				let name = cstr(name);
				(api.av_opt_set_int)(ctx, name.as_ptr(), value, 0)
			};
			// `sample_fmt` has no AVOption: its field, located at load.
			let at = ffmpeg.codec_sample_fmt.clone()?;
			write::<c_int>(ctx, at, fltp);
			let mut ret = set_int("ar", i64::from(RATE));
			if ret >= 0 {
				// `ch_layout` since libavcodec 59.24; before, a channel mask
				// (front left | front right) and a channel count.
				ret = match set("ch_layout", "stereo") {
					OPTION_NOT_FOUND => match set_int("channel_layout", 3) {
						r if r < 0 => r,
						_ => set_int("ac", 2),
					},
					r => r,
				};
			}
			if ret >= 0 {
				ret = set_int("b", i64::try_from(bitrate).unwrap_or(i64::MAX));
			}
			if ret >= 0 {
				let time_base = cstr("time_base");
				let tb = Rational { num: 1, den: RATE as c_int };
				ret = (api.av_opt_set_q)(ctx, time_base.as_ptr(), tb, 0);
			}
			if ret < 0 {
				return Err(format!("cannot set up the AAC encoder: {}", api.error_text(ret)));
			}
			let ret = (api.avcodec_open2)(ctx, codec, std::ptr::null_mut());
			if ret < 0 {
				return Err(format!("cannot open the AAC encoder: {}", api.error_text(ret)));
			}
		}
		Ok(this)
	}

	/// Decode one Opus packet of time `time` (48 kHz samples since any
	/// origin) and hand every AAC frame that is complete to `out`, with its
	/// time on the same clock (an AAC encoder starts 1024 samples early:
	/// its first frame is the codec's priming).
	///
	/// A packet that does not follow the last one (a gap in the studio's
	/// audio) starts the queue again at its time. A packet that does not
	/// decode is an error; the next one is decoded as usual.
	pub fn push(
		&mut self,
		opus: &[u8],
		time: i64,
		out: &mut dyn FnMut(&[u8], i64),
	) -> Result<(), String> {
		let api = &self.ffmpeg.api;
		// SAFETY: our packet points at `opus` only during the call that
		// copies it (the packet is not reference-counted), then is emptied
		// again; the decoder's frames are read within their sample count and
		// unreferenced after.
		unsafe {
			let packet = self.packet_in.cast::<PacketHead>();
			(*packet).data = opus.as_ptr().cast_mut();
			(*packet).size = c_int::try_from(opus.len()).map_err(|_| "Opus packet too large")?;
			(*packet).pts = NOPTS_VALUE;
			let ret = (api.avcodec_send_packet)(self.decoder, self.packet_in);
			(*packet).data = std::ptr::null_mut();
			(*packet).size = 0;
			if ret < 0 && ret != EAGAIN {
				return Err(format!("Opus decoding failed: {}", api.error_text(ret)));
			}
			let mut time = time;
			loop {
				let ret = (api.avcodec_receive_frame)(self.decoder, self.decoded);
				if ret == EAGAIN || ret == EOF {
					break;
				}
				if ret < 0 {
					return Err(format!("Opus decoding failed: {}", api.error_text(ret)));
				}
				let taken = self.take_decoded(time);
				let samples = i64::from((*self.decoded.cast::<FrameHead>()).nb_samples);
				(api.av_frame_unref)(self.decoded);
				taken?;
				time += samples;
			}
		}
		while self.pending[0].len() >= FRAME {
			self.encode_one(out)?;
		}
		Ok(())
	}

	/// Queue the samples of the decoded frame, which starts at `time`.
	///
	/// # Safety
	/// `self.decoded` must hold a frame from the decoder.
	unsafe fn take_decoded(&mut self, time: i64) -> Result<(), String> {
		let api = &self.ffmpeg.api;
		// SAFETY: guaranteed by the caller; planar float planes hold
		// `nb_samples` samples each.
		unsafe {
			let head = &*self.decoded.cast::<FrameHead>();
			if head.format != self.fltp {
				return Err(format!(
					"the Opus decoder gives sample format {}, not planar float",
					head.format
				));
			}
			let n = usize::try_from(head.nb_samples).unwrap_or(0);
			if n == 0 || head.data[0].is_null() {
				return Ok(());
			}
			if self.input.is_null() {
				let input = (api.av_frame_alloc)();
				if input.is_null() || (api.av_frame_ref)(input, self.decoded) < 0 {
					let mut input = input;
					(api.av_frame_free)(&mut input);
					return Err("cannot make the AAC encoder's frame".into());
				}
				self.input = input;
			}
			let expected = self.pending_time + self.pending[0].len() as i64;
			if self.pending[0].is_empty() || (time - expected).abs() > FRAME as i64 {
				self.pending[0].clear();
				self.pending[1].clear();
				self.pending_time = time;
			}
			let left = std::slice::from_raw_parts(head.data[0].cast::<f32>(), n);
			// A mono stream gives one plane: both channels get it.
			let right = if head.data[1].is_null() {
				left
			} else {
				std::slice::from_raw_parts(head.data[1].cast::<f32>(), n)
			};
			self.pending[0].extend_from_slice(left);
			self.pending[1].extend_from_slice(right);
		}
		Ok(())
	}

	/// Encode the first [`FRAME`] pending samples.
	fn encode_one(&mut self, out: &mut dyn FnMut(&[u8], i64)) -> Result<(), String> {
		let api = &self.ffmpeg.api;
		for (plane, pending) in self.planes.iter_mut().zip(&mut self.pending) {
			plane.copy_from_slice(&pending[..FRAME]);
			pending.drain(..FRAME);
		}
		let time = self.pending_time;
		self.pending_time += FRAME as i64;
		// SAFETY: `input` is a frame of ours (a reference to a decoded one,
		// see the module docs) whose leading fields we point at our planes,
		// which live as long as `self` and are not touched until the
		// encoder has taken the frame (it copies the samples when it
		// encodes, before `avcodec_receive_packet` asks for more input);
		// `pts` is the field located at load. Packets are read within their
		// size and unreferenced after.
		unsafe {
			let head = &mut *self.input.cast::<FrameHead>();
			for (data, plane) in head.data.iter_mut().zip(&mut self.planes) {
				*data = plane.as_mut_ptr().cast();
			}
			head.linesize[0] = (FRAME * size_of::<f32>()) as c_int;
			head.nb_samples = FRAME as c_int;
			write::<i64>(self.input, self.ffmpeg.frame.pts, time);
			let ret = (api.avcodec_send_frame)(self.encoder, self.input);
			if ret < 0 {
				return Err(format!("AAC encoding failed: {}", api.error_text(ret)));
			}
			loop {
				let ret = (api.avcodec_receive_packet)(self.encoder, self.packet_out);
				if ret == EAGAIN || ret == EOF {
					break;
				}
				if ret < 0 {
					return Err(format!("AAC encoding failed: {}", api.error_text(ret)));
				}
				let packet = &*self.packet_out.cast::<PacketHead>();
				let size = usize::try_from(packet.size).unwrap_or(0);
				if !packet.data.is_null() && size > 0 {
					out(std::slice::from_raw_parts(packet.data, size), packet.pts);
				}
				(api.av_packet_unref)(self.packet_out);
			}
		}
		Ok(())
	}
}

impl Drop for AacEncoder {
	fn drop(&mut self) {
		let api = &self.ffmpeg.api;
		// SAFETY: every pointer is NULL or an object of ours, freed once;
		// the free functions take NULL.
		unsafe {
			(api.av_frame_free)(&mut self.input);
			(api.av_frame_free)(&mut self.decoded);
			(api.av_packet_free)(&mut self.packet_in);
			(api.av_packet_free)(&mut self.packet_out);
			(api.avcodec_free_context)(&mut self.decoder);
			(api.avcodec_free_context)(&mut self.encoder);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_audio_specific_config_is_aac_lc_48k_stereo() {
		let config = u16::from_be_bytes(AUDIO_SPECIFIC_CONFIG);
		assert_eq!(config >> 11, 2, "object type");
		assert_eq!((config >> 7) & 0xF, 3, "sampling index (48 kHz)");
		assert_eq!((config >> 3) & 0xF, 2, "channels");
	}

	/// One second of a stereo tone as 20 ms Opus packets becomes AAC frames
	/// of 1024 samples, back to back from the first packet's time (less the
	/// encoder's 1024 samples of priming); a gap starts the clock again.
	#[test]
	fn opus_becomes_aac() {
		if Ffmpeg::get().is_err() {
			eprintln!("FFmpeg not installed, skipped");
			return;
		}
		let mut aac = AacEncoder::new(160_000).unwrap();
		let mut opus =
			opus2::Encoder::new(RATE, opus2::Channels::Stereo, opus2::Application::Audio).unwrap();
		let mut frames: Vec<(usize, i64)> = Vec::new();
		let mut packet = [0u8; 4000];
		let start = 48_000 * 7;
		let mut feed = |aac: &mut AacEncoder, from: i64, packets: i64, frames: &mut Vec<_>| {
			for p in 0..packets {
				let time = from + p * 960;
				let pcm: Vec<f32> = (0..960)
					.flat_map(|i| {
						let t = (time + i) as f32 / RATE as f32;
						let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
						[v, v]
					})
					.collect();
				let n = opus.encode_float(&pcm, &mut packet).unwrap();
				aac.push(&packet[..n], time, &mut |data, at| frames.push((data.len(), at)))
					.unwrap();
			}
		};
		feed(&mut aac, start, 50, &mut frames);
		assert!(frames.len() >= 44, "{} frames", frames.len());
		assert_eq!(frames[0].1, start - FRAME as i64, "{frames:?}");
		assert!(frames.windows(2).all(|w| w[1].1 - w[0].1 == FRAME as i64), "{frames:?}");
		assert!(frames.iter().all(|(size, _)| *size > 0));
		// Two seconds of silence nobody sent: the clock restarts.
		let after = start + 50 * 960 + 96_000;
		let before = frames.len();
		feed(&mut aac, after, 10, &mut frames);
		let restarted = frames[before..].iter().find(|(_, at)| *at >= after - FRAME as i64);
		assert!(restarted.is_some(), "{:?}", &frames[before..]);
	}
}
