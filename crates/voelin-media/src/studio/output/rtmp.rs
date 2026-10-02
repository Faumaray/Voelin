//! RTMP: the seam only.
//!
//! RTMP will go out through the FFmpeg libraries the crate already loads at
//! runtime ([`crate::ffmpeg`]): `libavformat` speaks `rtmp://` and
//! `rtmps://`, so the studio will hand it the same encoded packets it hands
//! every other output and never link or ship an RTMP stack of its own.
//!
//! What is missing is the muxer side of that loader (`avformat_alloc_output_
//! context2`, `av_interleaved_write_frame` and the bitstream filters H.264
//! and AAC need in FLV). Until then [`Rtmp::connect`] says so rather than
//! pretending, so a scene file or a `voelinctl` command that names an RTMP
//! output fails with a reason instead of silently dropping the stream.

use crate::studio::output::{OutputSink, Packet, Track};
use crate::{Error, Result};

/// An RTMP output. Not implemented yet; see the [module docs](self).
#[derive(Debug)]
pub struct Rtmp {
	url: String,
}

impl Rtmp {
	/// Whether `url` is one this output would take.
	pub fn handles(url: &str) -> bool {
		url.starts_with("rtmp://") || url.starts_with("rtmps://")
	}

	/// Always fails for now, with what is missing.
	pub fn connect(url: &str, _key: Option<&str>) -> Result<Self> {
		Err(Error::CodecUnavailable {
			codec: crate::codec::Codec::H264,
			reason: format!(
				"RTMP ({url}) needs the muxing side of the FFmpeg loader, which is not \
				 written yet; record to a file or use WHIP"
			),
		})
	}

	pub fn url(&self) -> &str {
		&self.url
	}
}

impl OutputSink for Rtmp {
	fn name(&self) -> &str {
		&self.url
	}

	fn wants(&self, _track: Track) -> bool {
		true
	}

	fn write(&mut self, _packet: &Packet<'_>) -> Result<()> {
		Err(Error::Capture { backend: "rtmp", message: "RTMP is not implemented".into() })
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rtmp_urls_are_recognised_but_refused() {
		assert!(Rtmp::handles("rtmp://live.example/app"));
		assert!(Rtmp::handles("rtmps://live.example/app"));
		assert!(!Rtmp::handles("https://example/whip"));
		let e = Rtmp::connect("rtmp://live.example/app", Some("key")).unwrap_err().to_string();
		assert!(e.contains("FFmpeg"), "{e}");
	}
}
