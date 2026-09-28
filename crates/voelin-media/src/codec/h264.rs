//! H.264 through Cisco's prebuilt OpenH264 library, loaded at runtime.
//!
//! Cisco pays the MPEG LA H.264 royalties only for its own binaries, and only
//! if each user downloads the binary themselves (not shipped with the app).
//! So the library is never compiled in: [`download_openh264`] fetches the
//! official build on the user's machine (feature `openh264-download`), and
//! [`OpenH264::load`] only accepts files whose SHA-256 matches a known Cisco
//! release. Apps using it must show [`OPENH264_ATTRIBUTION`] and let the user
//! disable it (see docs/media.md).
//!
//! TeamSpeak clients only decode H.264 Constrained High; OpenH264 encodes
//! without B-frames, so its High profile output qualifies.

use std::path::{Path, PathBuf};

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
	BitRate, Encoder, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode, UsageType,
};
use openh264::formats::{YUVSlices, YUVSource};

use crate::codec::{
	Codec, ContentHint, EncodedChunk, EncodedFrame, EncoderBackend, EncoderConfig, VideoDecoder,
	VideoEncoder,
};
use crate::convert;
use crate::frame::{FrameData, Plane, VideoFrame};
use crate::{Error, Result};

/// The notice Cisco's binary license requires in the app (e.g. the About
/// page).
pub const OPENH264_ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// The OpenH264 release we download and accept.
pub const OPENH264_VERSION: &str = "2.6.0";

/// Where Cisco publishes its binaries (bzip2 compressed, `<file>.bz2`).
pub const OPENH264_BASE_URL: &str = "https://ciscobinary.openh264.org/";

/// Cisco's OpenH264 2.6.0 builds: file name and SHA-256 of the decompressed
/// library (the list `openh264-sys2` also checks before loading).
#[cfg_attr(not(feature = "openh264-download"), allow(dead_code))]
const KNOWN_BUILDS: &[(&str, &str)] = &[
	(
		"libopenh264-2.6.0-linux64.8.so",
		"2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
	),
	(
		"libopenh264-2.6.0-linux32.8.so",
		"a46589ccc95df7565ff8b1722d3dead29c0809be28322dc763767e0aa35a6443",
	),
	(
		"libopenh264-2.6.0-linux-arm64.8.so",
		"12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
	),
	(
		"libopenh264-2.6.0-linux-arm.8.so",
		"df91866de0e93773019e30a8f2bdee8b15de4abe2bf89a228ae9f064ff1e85bb",
	),
	(
		"libopenh264-2.6.0-android-arm64.8.so",
		"4d9bc54d2d38e53eb7bd551ec61acb8ad8320d8b957bda751cfdbdcbfabc3b07",
	),
	(
		"libopenh264-2.6.0-android-arm.8.so",
		"45d8d77c119a488cad42549987a4a1ac50f634cfd6b08454c5565652e482022c",
	),
	(
		"libopenh264-2.6.0-android-x64.8.so",
		"548fa57935ff0eaf0a1d44c7963c1681daa5bf4cef2b34029fcd20a1e71adf81",
	),
	(
		"openh264-2.6.0-win64.dll",
		"2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691",
	),
	(
		"openh264-2.6.0-win32.dll",
		"b0098db6acbd290a1fe13997d61d461e7327e39b42bf868db41faf498b7621a2",
	),
	(
		"openh264-2.6.0-win-arm64.dll",
		"fb75103938f4f47d119b983e06334df41a803bc72fb5c46e3623f6fea5782732",
	),
	(
		"libopenh264-2.6.0-mac-arm64.dylib",
		"052e98bfcf7a9167d22f3bbb3f5988ef79065591f36af8b52924b22b13624551",
	),
	(
		"libopenh264-2.6.0-mac-x64.dylib",
		"e3dc8bc01fe69363f61fd3c02fd27798537a585eadd38cd808f303d1ee505a19",
	),
];

/// Cisco's file name for this platform, if Cisco builds one.
pub fn platform_library_name() -> Option<&'static str> {
	let name = match (std::env::consts::OS, std::env::consts::ARCH) {
		("linux", "x86_64") => "libopenh264-2.6.0-linux64.8.so",
		("linux", "x86") => "libopenh264-2.6.0-linux32.8.so",
		("linux", "aarch64") => "libopenh264-2.6.0-linux-arm64.8.so",
		("linux", "arm") => "libopenh264-2.6.0-linux-arm.8.so",
		("android", "aarch64") => "libopenh264-2.6.0-android-arm64.8.so",
		("android", "arm") => "libopenh264-2.6.0-android-arm.8.so",
		("android", "x86_64") => "libopenh264-2.6.0-android-x64.8.so",
		("windows", "x86_64") => "openh264-2.6.0-win64.dll",
		("windows", "x86") => "openh264-2.6.0-win32.dll",
		("windows", "aarch64") => "openh264-2.6.0-win-arm64.dll",
		("macos", "aarch64") => "libopenh264-2.6.0-mac-arm64.dylib",
		("macos", "x86_64") => "libopenh264-2.6.0-mac-x64.dylib",
		_ => return None,
	};
	Some(name)
}

#[cfg_attr(not(feature = "openh264-download"), allow(dead_code))]
fn expected_sha256(file_name: &str) -> Option<&'static str> {
	KNOWN_BUILDS.iter().find(|(name, _)| *name == file_name).map(|(_, sha)| *sha)
}

fn unavailable(reason: impl Into<String>) -> Error {
	Error::CodecUnavailable { codec: Codec::H264, reason: reason.into() }
}

/// A verified OpenH264 library on disk. Cheap to clone; each encoder and
/// decoder loads it (the OS shares the mapping).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenH264 {
	path: PathBuf,
}

impl OpenH264 {
	/// Use the library at `path`. Fails if it is missing, is not a known
	/// Cisco build, or cannot be loaded.
	pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
		let path = path.into();
		if !path.is_file() {
			return Err(unavailable(format!("OpenH264 library not found at {}", path.display())));
		}
		let library = Self { path };
		// Load once to verify the hash and the symbols.
		library.api()?;
		Ok(library)
	}

	/// Look for this platform's library in `dir` (where
	/// [`download_openh264`] puts it).
	pub fn find_in(dir: &Path) -> Result<Self> {
		let name = platform_library_name()
			.ok_or_else(|| unavailable("Cisco provides no OpenH264 build for this platform"))?;
		Self::load(dir.join(name))
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	fn api(&self) -> Result<OpenH264API> {
		// `from_blob_path` checks the SHA-256 against the known Cisco builds
		// before loading, so arbitrary files are never executed.
		OpenH264API::from_blob_path(&self.path).map_err(|e| {
			unavailable(format!("cannot load OpenH264 from {}: {e}", self.path.display()))
		})
	}

	pub fn encoder(&self, config: EncoderConfig) -> Result<OpenH264Encoder> {
		OpenH264Encoder::new(self.clone(), config)
	}

	pub fn decoder(&self) -> Result<OpenH264Decoder> {
		let decoder = Decoder::with_api_config(self.api()?, DecoderConfig::new())
			.map_err(|e| Error::Decoder { codec: Codec::H264, message: e.to_string() })?;
		Ok(OpenH264Decoder { decoder })
	}
}

pub use super::H264Profile;

/// OpenH264 encoder.
pub struct OpenH264Encoder {
	library: OpenH264,
	config: EncoderConfig,
	profile: H264Profile,
	encoder: Option<Encoder>,
	/// The last frame's bitstream (reused).
	output: Vec<u8>,
}

impl OpenH264Encoder {
	fn new(library: OpenH264, config: EncoderConfig) -> Result<Self> {
		let mut encoder = Self {
			library,
			profile: config.h264_profile,
			config,
			encoder: None,
			output: Vec::new(),
		};
		encoder.encoder = Some(encoder.create()?);
		Ok(encoder)
	}

	pub fn set_profile(&mut self, profile: H264Profile) -> Result<()> {
		self.profile = profile;
		self.encoder = Some(self.create()?);
		Ok(())
	}

	fn create(&self) -> Result<Encoder> {
		let usage = match self.config.content {
			ContentHint::Screen => UsageType::ScreenContentRealTime,
			ContentHint::Motion => UsageType::CameraVideoRealTime,
		};
		let profile = match self.profile {
			H264Profile::ConstrainedHigh => Profile::High,
			H264Profile::ConstrainedBaseline => Profile::Baseline,
		};
		let config = openh264::encoder::EncoderConfig::new()
			.bitrate(BitRate::from_bps(self.config.bitrate_bps))
			.max_frame_rate(FrameRate::from_hz(self.config.fps as f32))
			.rate_control_mode(RateControlMode::Bitrate)
			.usage_type(usage)
			.profile(profile)
			// Rate control only holds the bitrate if it may skip frames.
			.skip_frames(true)
			// Not supported for screen content; OpenH264 would warn and drop them.
			.adaptive_quantization(self.config.content == ContentHint::Motion)
			.background_detection(self.config.content == ContentHint::Motion)
			.intra_frame_period(IntraFramePeriod::from_num_frames(
				self.config.keyframe_interval.unwrap_or(0),
			))
			.num_threads(self.config.threads.min(u32::from(u16::MAX)) as u16);
		Encoder::with_api_config(self.library.api()?, config).map_err(|e| self.error(e))
	}

	fn error(&self, e: openh264::Error) -> Error {
		Error::Encoder { codec: Codec::H264, message: e.to_string() }
	}
}

impl VideoEncoder for OpenH264Encoder {
	fn codec(&self) -> Codec {
		Codec::H264
	}

	fn backend(&self) -> EncoderBackend {
		EncoderBackend::OpenH264
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

	/// Writes the NAL units into a buffer kept across frames.
	fn encode_with(
		&mut self,
		frame: &VideoFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let i420 = convert::to_i420(frame)?;
		let FrameData::I420 { y, u, v } = &i420.data else {
			unreachable!("to_i420 returns I420");
		};
		let source = YUVSlices::new(
			(&y.data, &u.data, &v.data),
			(i420.width as usize, i420.height as usize),
			(y.stride, u.stride, v.stride),
		);
		let encoder = self.encoder.as_mut().expect("created in new");
		if force_keyframe {
			encoder.force_intra_frame();
		}
		let timestamp = openh264::Timestamp::from_millis(i420.timestamp.as_millis() as u64);
		let bitstream = match encoder.encode_at(&source, timestamp) {
			Ok(b) => b,
			Err(e) => return Err(Error::Encoder { codec: Codec::H264, message: e.to_string() }),
		};
		let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
		if matches!(bitstream.frame_type(), FrameType::Skip) {
			return Ok(());
		}
		self.output.clear();
		bitstream.write_vec(&mut self.output);
		if !self.output.is_empty() {
			out(EncodedChunk { data: &self.output, keyframe, pts_90khz: i420.pts_90khz() });
		}
		Ok(())
	}

	/// The `openh264` crate has no safe way to change the bitrate of a
	/// running encoder, so this recreates it (the next frame is an IDR).
	fn set_bitrate(&mut self, bps: u32) -> Result<()> {
		if bps == self.config.bitrate_bps {
			return Ok(());
		}
		self.config.bitrate_bps = bps;
		self.encoder = Some(self.create()?);
		Ok(())
	}
}

/// OpenH264 decoder (Annex B input, as depacketized by str0m).
pub struct OpenH264Decoder {
	decoder: Decoder,
}

impl VideoDecoder for OpenH264Decoder {
	fn codec(&self) -> Codec {
		Codec::H264
	}

	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
		let picture = self
			.decoder
			.decode(data)
			.map_err(|e| Error::Decoder { codec: Codec::H264, message: e.to_string() })?;
		let Some(picture) = picture else { return Ok(None) };
		let (w, h) = picture.dimensions();
		let (sy, su, sv) = picture.strides();
		let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
		let copy = |data: &[u8], stride: usize, width: usize, rows: usize| {
			let mut out = Vec::with_capacity(width * rows);
			for row in 0..rows {
				out.extend_from_slice(&data[row * stride..row * stride + width]);
			}
			Plane::new(out, width)
		};
		Ok(Some(VideoFrame {
			width: w as u32,
			height: h as u32,
			timestamp: std::time::Duration::ZERO,
			data: FrameData::I420 {
				y: copy(picture.y(), sy, w, h),
				u: copy(picture.u(), su, cw, ch),
				v: copy(picture.v(), sv, cw, ch),
			},
		}))
	}
}

/// Download Cisco's OpenH264 build for this platform into `dir` (created if
/// needed) over HTTPS, check its SHA-256, and load it. Reuses a verified
/// file that is already there.
///
/// Call this only after the user agreed (e.g. a settings switch that shows
/// [`OPENH264_ATTRIBUTION`]); Cisco's license requires the download to happen
/// on the user's machine.
#[cfg(feature = "openh264-download")]
pub async fn download_openh264(dir: &Path) -> Result<OpenH264> {
	download_from(OPENH264_BASE_URL, dir).await
}

/// [`download_openh264`] from another base URL (a mirror, or a local server
/// in tests). The SHA-256 check is the same.
#[cfg(feature = "openh264-download")]
pub async fn download_from(base_url: &str, dir: &Path) -> Result<OpenH264> {
	use std::io::Read;

	use sha2::Digest;

	let name = platform_library_name()
		.ok_or_else(|| unavailable("Cisco provides no OpenH264 build for this platform"))?;
	let expected = expected_sha256(name).expect("every platform name has a hash");
	let target = dir.join(name);
	if target.is_file() {
		match OpenH264::load(&target) {
			Ok(library) => return Ok(library),
			Err(e) => tracing::info!("replacing {}: {e}", target.display()),
		}
	}

	let url = format!("{}/{name}.bz2", base_url.trim_end_matches('/'));
	let download = |e: &dyn std::fmt::Display| Error::Download(format!("{url}: {e}"));
	let response = reqwest::get(&url).await.map_err(|e| download(&e))?;
	let response = response.error_for_status().map_err(|e| download(&e))?;
	let compressed = response.bytes().await.map_err(|e| download(&e))?;
	// The libraries are about 1-2 MB; refuse anything absurd.
	let mut library = Vec::new();
	bzip2::read::BzDecoder::new(&compressed[..])
		.take(64 << 20)
		.read_to_end(&mut library)
		.map_err(|e| download(&format!("bad bzip2 data: {e}")))?;
	let actual: String =
		sha2::Sha256::digest(&library).iter().map(|b| format!("{b:02x}")).collect();
	if actual != expected {
		return Err(download(&format!("SHA-256 mismatch: got {actual}, expected {expected}")));
	}

	std::fs::create_dir_all(dir)?;
	let partial = dir.join(format!("{name}.part"));
	std::fs::write(&partial, &library)?;
	std::fs::rename(&partial, &target)?;
	OpenH264::load(target)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn missing_library_is_unavailable() {
		let err = OpenH264::load("/nonexistent/libopenh264.so").unwrap_err();
		assert!(matches!(err, Error::CodecUnavailable { codec: Codec::H264, .. }), "{err}");
		assert!(err.to_string().contains("not found"), "{err}");

		let dir = std::env::temp_dir().join(format!("voelin-media-h264-{}", std::process::id()));
		assert!(OpenH264::find_in(&dir).is_err());
	}

	#[test]
	fn unknown_file_is_rejected() {
		let dir =
			std::env::temp_dir().join(format!("voelin-media-h264-bad-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let fake = dir.join(platform_library_name().unwrap_or("libopenh264.so"));
		std::fs::write(&fake, b"not a library").unwrap();
		let err = OpenH264::load(&fake).unwrap_err();
		std::fs::remove_dir_all(&dir).unwrap();
		assert!(matches!(err, Error::CodecUnavailable { .. }), "{err}");
		assert!(err.to_string().to_lowercase().contains("hash"), "{err}");
	}

	#[test]
	fn platform_names_have_hashes() {
		if let Some(name) = platform_library_name() {
			assert!(expected_sha256(name).is_some());
		}
		assert!(KNOWN_BUILDS.iter().all(|(_, sha)| sha.len() == 64));
	}
}
