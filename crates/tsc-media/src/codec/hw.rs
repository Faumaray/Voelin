//! Hardware video encoders.
//!
//! The streamer prefers a GPU encoder over software VP8 / OpenH264. None is
//! implemented yet; the factories below mark where they plug in:
//!
//! - Linux: VA-API (H.264 Constrained High / VP8 / VP9 on Intel and AMD), e.g.
//!   through `cros-libva`. TODO.
//! - Windows: Media Foundation H.264 encoder MFT (NVENC / QuickSync / AMF
//!   behind it). TODO.

use crate::Result;
use crate::codec::{Codec, EncoderConfig, VideoEncoder};

/// Creates encoders of one hardware API.
pub trait HardwareEncoderFactory: Send + Sync {
	/// Short name for logs and settings (`"vaapi"`, `"mediafoundation"`).
	fn name(&self) -> &'static str;

	/// Codecs this device can encode, best first.
	fn codecs(&self) -> Vec<Codec>;

	fn create(&self, codec: Codec, config: &EncoderConfig) -> Result<Box<dyn VideoEncoder>>;
}

/// Hardware encoders usable on this machine (none yet).
pub fn probe() -> Vec<Box<dyn HardwareEncoderFactory>> {
	let mut found: Vec<Box<dyn HardwareEncoderFactory>> = Vec::new();
	#[cfg(target_os = "linux")]
	if let Some(f) = vaapi::probe() {
		found.push(Box::new(f));
	}
	#[cfg(windows)]
	if let Some(f) = media_foundation::probe() {
		found.push(Box::new(f));
	}
	found
}

#[cfg(target_os = "linux")]
mod vaapi {
	use super::*;
	use crate::Error;

	pub struct Vaapi;

	/// TODO: open the DRM render node, query VAProfileH264ConstrainedBaseline /
	/// VAProfileH264High / VP8 / VP9 encode entrypoints.
	pub fn probe() -> Option<Vaapi> {
		None
	}

	impl HardwareEncoderFactory for Vaapi {
		fn name(&self) -> &'static str {
			"vaapi"
		}

		fn codecs(&self) -> Vec<Codec> {
			Vec::new()
		}

		fn create(&self, codec: Codec, _: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
			Err(Error::CodecUnavailable {
				codec,
				reason: "VA-API encoding is not implemented".into(),
			})
		}
	}
}

#[cfg(windows)]
mod media_foundation {
	use super::*;
	use crate::Error;

	pub struct MediaFoundation;

	/// TODO: enumerate hardware H.264 encoder MFTs (`MFTEnumEx` with
	/// `MFT_ENUM_FLAG_HARDWARE`).
	pub fn probe() -> Option<MediaFoundation> {
		None
	}

	impl HardwareEncoderFactory for MediaFoundation {
		fn name(&self) -> &'static str {
			"mediafoundation"
		}

		fn codecs(&self) -> Vec<Codec> {
			Vec::new()
		}

		fn create(&self, codec: Codec, _: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
			Err(Error::CodecUnavailable {
				codec,
				reason: "Media Foundation encoding is not implemented".into(),
			})
		}
	}
}
