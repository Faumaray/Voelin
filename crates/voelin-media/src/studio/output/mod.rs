//! Where the studio's encoded packets go besides the stream.
//!
//! The streamer already encodes every layer; an [`OutputSink`] is fed the
//! same packets, so recording, the replay buffer and WHIP cost no second
//! encode and work whether or not the stream is live.
//!
//! - [`record`]: a file, WebM or Matroska ([`ebml`])
//! - [`replay`]: the last few seconds in memory (spilling to disk), written
//!   out as a clip without re-encoding
//! - [`whip`]: WHIP (WebRTC-HTTP ingestion) to a broadcast service (feature
//!   `whip`)
//! - [`rtmp`]: the seam for RTMP, which will go through the FFmpeg loader
//!
//! A sink never stalls the encoder: [`OutputSink::write`] returns an error
//! instead of blocking for long, and the studio drops the sink and reports
//! it.

use crate::Result;
use crate::codec::Codec;

pub mod ebml;
pub mod record;
pub mod replay;
pub mod rtmp;
#[cfg(feature = "whip")]
pub mod whip;

/// Which stream a packet belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Track {
	/// One simulcast layer's video.
	Video { codec: Codec, layer: u32 },
	/// The studio's audio: Opus, 48 kHz.
	Audio { channels: u16 },
}

impl Track {
	pub fn is_video(&self) -> bool {
		matches!(self, Self::Video { .. })
	}

	pub fn layer(&self) -> u32 {
		match self {
			Self::Video { layer, .. } => *layer,
			Self::Audio { .. } => u32::MAX,
		}
	}
}

/// One encoded packet on its way out of the studio.
#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
	pub track: Track,
	/// Presentation time on the 90 kHz clock, as RTP carries it.
	pub pts_90khz: u64,
	/// Video: a frame that stands alone. Audio: always true.
	pub keyframe: bool,
	/// Size of the picture this packet belongs to (video only, 0 otherwise).
	pub width: u32,
	pub height: u32,
	pub data: &'a [u8],
}

impl Packet<'_> {
	/// The packet's time in milliseconds.
	pub fn ms(&self) -> u64 {
		self.pts_90khz * 1000 / 90_000
	}
}

/// Something fed with the studio's encoded packets.
///
/// Every method runs on the encoder's thread, so none of them may block for
/// long; work that can wait belongs on the sink's own thread.
pub trait OutputSink: Send {
	/// For logs, stats and the controller's events.
	fn name(&self) -> &str;

	/// Whether this sink wants `track` at all. A recording takes one layer;
	/// WHIP may take another.
	fn wants(&self, track: Track) -> bool {
		let _ = track;
		true
	}

	/// Whether the sink needs a keyframe now (it has just started, or it
	/// dropped packets).
	fn needs_keyframe(&mut self) -> bool {
		false
	}

	/// Take one packet.
	fn write(&mut self, packet: &Packet<'_>) -> Result<()>;

	/// Close the file, tear the session down. Called once.
	fn finish(&mut self) -> Result<()> {
		Ok(())
	}

	/// Bytes written so far, for the stats.
	fn bytes(&self) -> u64 {
		0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn packet_times_and_tracks() {
		let packet = Packet {
			track: Track::Video { codec: Codec::Vp8, layer: 2 },
			pts_90khz: 90_000 * 3 / 2,
			keyframe: true,
			width: 8,
			height: 6,
			data: &[1],
		};
		assert_eq!(packet.ms(), 1500);
		assert!(packet.track.is_video());
		assert_eq!(packet.track.layer(), 2);
		assert!(!Track::Audio { channels: 2 }.is_video());
	}
}
