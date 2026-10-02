//! FFmpeg, loaded at runtime (feature `ffmpeg`): hardware video encoders
//! (VA-API, NVENC, Quick Sync, AMF, Media Foundation, VideoToolbox) and
//! more software encoders (x264, OpenH264, SVT-AV1, rav1e, libaom).
//!
//! FFmpeg is never linked or shipped: [`Ffmpeg::get`] finds the system's
//! (or `VOELIN_FFMPEG_DIR`'s) libavcodec and libavutil, whatever their major
//! version, and looks up the functions it needs by name ([`sys`]). Without
//! them the software encoders of this crate are used as before. Struct
//! layouts that differ between releases are avoided (AVOptions for every
//! codec setting) or located and checked at runtime ([`layout`]).
//!
//! [`probe`] lists every encoder backend with the result of a self-test (one
//! small frame encoded), so a backend whose driver or GPU is missing is
//! skipped with a reason, without affecting the others.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub mod encoder;
mod layout;
pub mod sys;
mod vpp;

pub use encoder::{
	BACKENDS, BackendKind, BackendSpec, BackendStatus, FfmpegEncoder, FfmpegFactory, Input, probe,
};
pub use sys::Search;

use self::layout::{FrameFields, FrameRefs};
use self::sys::{Api, Libs, LogCallback, Ptr, VaList};

/// Pixel format numbers of this release (looked up by name).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PixFmts {
	pub yuv420p: c_int,
	pub nv12: c_int,
	pub vaapi: Option<c_int>,
	pub drm_prime: Option<c_int>,
	/// `bgr0`: B, G, R, x in memory, the DRM `XR24` of captured screens.
	pub bgr0: Option<c_int>,
}

/// What was loaded, for logs and the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryInfo {
	/// libavcodec's file.
	pub path: PathBuf,
	/// FFmpeg's release (`av_version_info`, e.g. `6.1.1-3ubuntu5`).
	pub release: String,
	/// `major.minor.micro` of libavcodec and libavutil.
	pub avcodec: String,
	pub avutil: String,
	/// libavformat of the same release was found (for RTMP output later).
	pub avformat: bool,
	/// Why hardware frame pools (VA-API) cannot be used, if not.
	pub hw_frames: Result<(), String>,
	/// Why DMA-BUF frames cannot be imported (zero-copy), if not.
	pub dmabuf_import: Result<(), String>,
}

/// Loaded FFmpeg libraries.
pub struct Ffmpeg {
	pub(crate) api: Api,
	_libs: Libs,
	pub(crate) frame: FrameFields,
	/// `AVCodecContext.hw_frames_ctx`.
	pub(crate) codec_hw_frames: Result<usize, String>,
	/// `AVFrame.buf[0]` / `hw_frames_ctx`.
	pub(crate) frame_refs: Result<FrameRefs, String>,
	pub(crate) pix: PixFmts,
	info: LibraryInfo,
}

// SAFETY: `Api` holds plain function pointers and `Libs` library handles;
// FFmpeg's functions used here are thread-safe for distinct objects.
unsafe impl Send for Ffmpeg {}
unsafe impl Sync for Ffmpeg {}

impl std::fmt::Debug for Ffmpeg {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Ffmpeg").field("info", &self.info).finish_non_exhaustive()
	}
}

fn version_text(v: u32) -> String {
	format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

impl Ffmpeg {
	/// Load FFmpeg from `search`. Prefer [`Ffmpeg::get`], which loads once
	/// per process.
	pub fn load(search: &Search) -> Result<Self, String> {
		let (libs, api) = sys::load(search)?;
		let frame = layout::frame_fields(&api)?;
		let yuv420p = api.pix_fmt("yuv420p").ok_or("no yuv420p pixel format")?;
		let nv12 = api.pix_fmt("nv12").ok_or("no nv12 pixel format")?;
		let pix = PixFmts {
			yuv420p,
			nv12,
			vaapi: api.pix_fmt("vaapi").or_else(|| api.pix_fmt("vaapi_vld")),
			drm_prime: api.pix_fmt("drm_prime"),
			bgr0: api.pix_fmt("bgr0"),
		};
		let codec_hw_frames = layout::codec_hw_frames(&api);
		let frame_refs = layout::frame_refs(&api, yuv420p);
		// SAFETY: no arguments; returns version numbers and a static string.
		let (avcodec, avutil, release) = unsafe {
			(
				(api.avcodec_version)(),
				(api.avutil_version)(),
				CStr::from_ptr((api.av_version_info)()).to_string_lossy().into_owned(),
			)
		};
		let info = LibraryInfo {
			path: libs.path.clone(),
			release,
			avcodec: version_text(avcodec),
			avutil: version_text(avutil),
			avformat: libs.avformat.is_some(),
			hw_frames: codec_hw_frames.clone().map(|_| ()),
			dmabuf_import: match (&codec_hw_frames, &frame_refs, pix.drm_prime) {
				(Err(e), _, _) | (_, Err(e), _) => Err(e.clone()),
				(_, _, None) => Err("no DRM PRIME pixel format".into()),
				_ => Ok(()),
			},
		};
		Ok(Self { api, _libs: libs, frame, codec_hw_frames, frame_refs, pix, info })
	}

	/// The process's FFmpeg, loaded on first use from [`Search::from_env`]
	/// (`VOELIN_FFMPEG_DIR`), or why there is none. `VOELIN_FFMPEG=0`
	/// disables it.
	pub fn get() -> Result<&'static Ffmpeg, &'static str> {
		static FFMPEG: OnceLock<Result<Ffmpeg, String>> = OnceLock::new();
		FFMPEG
			.get_or_init(|| {
				if std::env::var("VOELIN_FFMPEG").is_ok_and(|v| matches!(v.as_str(), "0" | "off")) {
					return Err("disabled by VOELIN_FFMPEG".into());
				}
				let loaded = Self::load(&Search::from_env());
				match &loaded {
					Ok(ffmpeg) => {
						ffmpeg.capture_log();
						tracing::info!(
							path = %ffmpeg.info.path.display(),
							release = ffmpeg.info.release,
							avcodec = ffmpeg.info.avcodec,
							"FFmpeg loaded"
						);
					}
					Err(e) => tracing::info!("FFmpeg not used: {e}"),
				}
				loaded
			})
			.as_ref()
			.map_err(String::as_str)
	}

	pub fn info(&self) -> &LibraryInfo {
		&self.info
	}

	/// Route FFmpeg's warnings and errors to `tracing` (and to
	/// [`take_log`] for error reports) instead of stderr.
	fn capture_log(&self) {
		let _ = LOG_FORMAT.set(self.api.av_log_format_line2);
		// SAFETY: sets a global level and a callback that stays valid (a
		// plain function).
		unsafe {
			(self.api.av_log_set_level)(sys::LOG_WARNING);
			(self.api.av_log_set_callback)(Some(log_callback as LogCallback));
		}
	}
}

type FormatLine = unsafe extern "C" fn(
	Ptr,
	c_int,
	*const c_char,
	VaList,
	*mut c_char,
	c_int,
	*mut c_int,
) -> c_int;

static LOG_FORMAT: OnceLock<FormatLine> = OnceLock::new();

/// The last FFmpeg messages (warnings and errors), for error reports.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Take the FFmpeg messages logged since the last call (at most 16) that
/// are about `context` (an encoder name such as `h264_vaapi`: FFmpeg
/// prefixes its messages with `[h264_vaapi @ 0x...]`) or about nothing in
/// particular. Messages of other encoders (tested in parallel) stay.
pub fn take_log(context: &str) -> Vec<String> {
	let mut log = LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
	let tag = format!("[{context} @");
	let (mine, others): (Vec<String>, Vec<String>) =
		log.drain(..).partition(|line| line.contains(&tag) || !line.contains(" @ 0x"));
	*log = others;
	mine
}

unsafe extern "C" fn log_callback(avcl: Ptr, level: c_int, fmt: *const c_char, vl: VaList) {
	if level > sys::LOG_WARNING {
		return;
	}
	let Some(format) = LOG_FORMAT.get() else { return };
	let mut line = [0 as c_char; 512];
	let mut prefix: c_int = 1;
	// SAFETY: FFmpeg passes its own arguments through; the buffer and its
	// size are valid, and the result is NUL-terminated.
	let text = unsafe {
		format(avcl, level, fmt, vl, line.as_mut_ptr(), line.len() as c_int, &mut prefix);
		CStr::from_ptr(line.as_ptr())
	};
	let text = text.to_string_lossy().trim_end().to_owned();
	if text.is_empty() {
		return;
	}
	if level <= sys::LOG_ERROR {
		tracing::debug!(target: "ffmpeg", "{text}");
	} else {
		tracing::trace!(target: "ffmpeg", "{text}");
	}
	if let Ok(mut log) = LOG.lock() {
		if log.len() >= 16 {
			log.remove(0);
		}
		log.push(text);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// With FFmpeg installed (the CI image and this machine have it), every
	/// runtime check of the layout passes.
	#[test]
	fn loads_when_installed() {
		let Ok(ffmpeg) = Ffmpeg::get() else {
			eprintln!("FFmpeg not installed, skipped");
			return;
		};
		let info = ffmpeg.info();
		assert!(!info.release.is_empty());
		assert!(info.avcodec.split('.').count() == 3);
		assert!(info.hw_frames.is_ok(), "{info:?}");
		if cfg!(target_pointer_width = "64") {
			assert!(ffmpeg.frame_refs.is_ok(), "{:?}", ffmpeg.frame_refs);
		}
	}

	#[test]
	fn no_ffmpeg_in_an_empty_directory() {
		let dir = std::env::temp_dir().join(format!("voelin-no-ffmpeg-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let err = Ffmpeg::load(&Search { dirs: vec![dir.clone()], system: false }).unwrap_err();
		std::fs::remove_dir_all(&dir).unwrap();
		assert!(err.contains("no FFmpeg libraries"), "{err}");
	}
}
