//! A small Matroska / WebM writer, enough for what the studio records.
//!
//! Only what a recording needs is written: an EBML header, one `Info`, one
//! `Tracks` and a stream of `Cluster`s of `SimpleBlock`s. The `Segment` has
//! an unknown size and there are no `Cues`, so a recording that is cut short
//! (a crash, a full disk) still plays up to the last cluster; players seek by
//! scanning, which is what a live WebM stream needs anyway. The one thing
//! written back is the `Duration`, which is reserved in the header and filled
//! in on [`Writer::finish`] -- without it players show a file of unknown
//! length.
//!
//! Timestamps are milliseconds (`TimecodeScale` 1 ms); each cluster starts at
//! a keyframe, and blocks carry their offset from the cluster as a 16-bit
//! value, so a cluster never spans more than 32 seconds.
//!
//! H.264 has to be stored length-prefixed with its parameter sets in
//! `CodecPrivate` ([`avcc`]), which is why the header is written when the
//! first keyframe arrives rather than at the start.

use std::io::{Seek, SeekFrom, Write};

use crate::codec::Codec;
use crate::{Error, Result};

/// `Segment`, `Cluster` and the rest, as their full EBML ids.
mod id {
	pub const EBML: u32 = 0x1A45_DFA3;
	pub const EBML_VERSION: u32 = 0x4286;
	pub const EBML_READ_VERSION: u32 = 0x42F7;
	pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
	pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
	pub const DOC_TYPE: u32 = 0x4282;
	pub const DOC_TYPE_VERSION: u32 = 0x4287;
	pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;
	pub const SEGMENT: u32 = 0x1853_8067;
	pub const INFO: u32 = 0x1549_A966;
	pub const TIMECODE_SCALE: u32 = 0x002A_D7B1;
	pub const DURATION: u32 = 0x4489;
	pub const MUXING_APP: u32 = 0x4D80;
	pub const WRITING_APP: u32 = 0x5741;
	pub const TRACKS: u32 = 0x1654_AE6B;
	pub const TRACK_ENTRY: u32 = 0xAE;
	pub const TRACK_NUMBER: u32 = 0xD7;
	pub const TRACK_UID: u32 = 0x73C5;
	pub const TRACK_TYPE: u32 = 0x83;
	pub const FLAG_LACING: u32 = 0x9C;
	pub const CODEC_ID: u32 = 0x86;
	pub const CODEC_PRIVATE: u32 = 0x63A2;
	pub const LANGUAGE: u32 = 0x0022_B59C;
	pub const VIDEO: u32 = 0xE0;
	pub const PIXEL_WIDTH: u32 = 0xB0;
	pub const PIXEL_HEIGHT: u32 = 0xBA;
	pub const AUDIO: u32 = 0xE1;
	pub const SAMPLING_FREQUENCY: u32 = 0xB5;
	pub const CHANNELS: u32 = 0x9F;
	pub const CLUSTER: u32 = 0x1F43_B675;
	pub const TIMECODE: u32 = 0xE7;
	pub const SIMPLE_BLOCK: u32 = 0xA3;
}

/// Append an element id (its bytes already carry their length).
fn put_id(out: &mut Vec<u8>, id: u32) {
	let bytes = id.to_be_bytes();
	let first = bytes.iter().position(|&b| b != 0).unwrap_or(3);
	out.extend_from_slice(&bytes[first..]);
}

/// Append an EBML unsigned integer of `len` bytes (the length marker is the
/// top bit of the first byte).
fn put_size(out: &mut Vec<u8>, size: u64) {
	// The shortest length whose value range holds `size` (all-ones is
	// reserved for "unknown").
	let mut len = 1;
	while len < 8 && size >= (1u64 << (7 * len)) - 1 {
		len += 1;
	}
	let marked = size | 1u64 << (7 * len);
	out.extend_from_slice(&marked.to_be_bytes()[8 - len..]);
}

/// Append an element with `body` as its content.
fn element(out: &mut Vec<u8>, id: u32, body: &[u8]) {
	put_id(out, id);
	put_size(out, body.len() as u64);
	out.extend_from_slice(body);
}

/// An unsigned integer element, in as few bytes as it fits.
fn uint(out: &mut Vec<u8>, id: u32, value: u64) {
	let bytes = value.to_be_bytes();
	let first = bytes.iter().position(|&b| b != 0).unwrap_or(7);
	element(out, id, &bytes[first..]);
}

/// An IEEE-754 double element (Matroska stores frequencies as floats).
fn float(out: &mut Vec<u8>, id: u32, value: f64) {
	element(out, id, &value.to_be_bytes());
}

fn text(out: &mut Vec<u8>, id: u32, value: &str) {
	element(out, id, value.as_bytes());
}

/// What one track holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
	Video {
		codec: Codec,
		width: u32,
		height: u32,
	},
	/// Opus at 48 kHz.
	Opus {
		channels: u16,
	},
}

/// One track of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
	pub kind: TrackKind,
	/// `CodecPrivate`: the Opus head, or H.264's `avcC`.
	pub private: Vec<u8>,
}

impl TrackKind {
	/// The Matroska codec id.
	fn codec_id(&self) -> &'static str {
		match self {
			Self::Video { codec: Codec::Vp8, .. } => "V_VP8",
			Self::Video { codec: Codec::Vp9, .. } => "V_VP9",
			Self::Video { codec: Codec::Av1, .. } => "V_AV1",
			Self::Video { codec: Codec::H264, .. } => "V_MPEG4/ISO/AVC",
			Self::Video { codec: Codec::H265, .. } => "V_MPEGH/ISO/HEVC",
			Self::Opus { .. } => "A_OPUS",
		}
	}

	/// Whether WebM allows this codec (Matroska allows everything).
	fn in_webm(&self) -> bool {
		matches!(
			self,
			Self::Video { codec: Codec::Vp8 | Codec::Vp9 | Codec::Av1, .. } | Self::Opus { .. }
		)
	}
}

/// The `CodecPrivate` of an Opus track: an `OpusHead` for 48 kHz.
pub fn opus_head(channels: u16) -> Vec<u8> {
	let mut head = Vec::with_capacity(19);
	head.extend_from_slice(b"OpusHead");
	head.push(1); // version
	head.push(channels.clamp(1, 255) as u8);
	head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
	head.extend_from_slice(&48_000u32.to_le_bytes());
	head.extend_from_slice(&0i16.to_le_bytes()); // output gain
	head.push(0); // channel mapping family
	head
}

/// The NAL units of an Annex B stream (start codes stripped).
pub fn annex_b_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
	let mut rest = data;
	std::iter::from_fn(move || {
		// Skip to the first start code.
		let start = (0..rest.len().saturating_sub(2)).find(|&i| rest[i..i + 3] == [0, 0, 1])?;
		rest = &rest[start + 3..];
		let end = (0..rest.len().saturating_sub(2))
			.find(|&i| rest[i..i + 3] == [0, 0, 1])
			.map_or(rest.len(), |i| if i > 0 && rest[i - 1] == 0 { i - 1 } else { i });
		let (unit, tail) = rest.split_at(end);
		rest = tail;
		Some(unit)
	})
	.filter(|unit| !unit.is_empty())
}

/// The `avcC` record of an Annex B keyframe: the parameter sets Matroska
/// needs before any picture. `None` if the frame carries none.
pub fn avcc(keyframe: &[u8]) -> Option<Vec<u8>> {
	let mut sps: Option<&[u8]> = None;
	let mut pps: Option<&[u8]> = None;
	for unit in annex_b_units(keyframe) {
		match unit[0] & 0x1F {
			7 => sps = sps.or(Some(unit)),
			8 => pps = pps.or(Some(unit)),
			_ => {}
		}
	}
	let (sps, pps) = (sps?, pps?);
	if sps.len() < 4 {
		return None;
	}
	let mut record = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
	record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
	record.extend_from_slice(sps);
	record.push(1);
	record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
	record.extend_from_slice(pps);
	Some(record)
}

/// An Annex B frame as the length-prefixed units Matroska stores.
pub fn to_length_prefixed(data: &[u8], out: &mut Vec<u8>) {
	out.clear();
	for unit in annex_b_units(data) {
		// Access unit delimiters say nothing a container does not.
		if unit[0] & 0x1F == 9 {
			continue;
		}
		out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
		out.extend_from_slice(unit);
	}
}

/// A cluster is closed at a keyframe, or once it reaches this many bytes or
/// this much time (a block's timestamp is 16 bits from the cluster's).
const CLUSTER_BYTES: usize = 4 << 20;
const CLUSTER_MS: u64 = 5_000;

/// Writes Matroska or WebM; see the [module docs](self).
pub struct Writer<W: Write + Seek> {
	out: W,
	/// Where the `Duration` value sits, to fill in at the end.
	duration_at: u64,
	tracks: Vec<Track>,
	/// Timestamp of the open cluster, and its blocks.
	cluster: Option<u64>,
	buffer: Vec<u8>,
	/// The first timestamp seen, so the file starts at zero.
	origin: Option<u64>,
	last_ms: u64,
	bytes: u64,
	blocks: u64,
}

impl<W: Write + Seek> Writer<W> {
	/// Start a file with these tracks (track 1 first). `webm` writes the
	/// WebM doc type, which only allows VP8, VP9, AV1 and Opus.
	pub fn new(mut out: W, webm: bool, tracks: Vec<Track>) -> Result<Self> {
		if tracks.is_empty() {
			return Err(Error::InvalidFrame("a recording needs at least one track".into()));
		}
		if webm && let Some(track) = tracks.iter().find(|t| !t.kind.in_webm()) {
			return Err(Error::InvalidFrame(format!(
				"{} cannot go into WebM; record Matroska instead",
				track.kind.codec_id()
			)));
		}
		let mut head = Vec::new();
		let mut body = Vec::new();
		uint(&mut body, id::EBML_VERSION, 1);
		uint(&mut body, id::EBML_READ_VERSION, 1);
		uint(&mut body, id::EBML_MAX_ID_LENGTH, 4);
		uint(&mut body, id::EBML_MAX_SIZE_LENGTH, 8);
		text(&mut body, id::DOC_TYPE, if webm { "webm" } else { "matroska" });
		uint(&mut body, id::DOC_TYPE_VERSION, 4);
		uint(&mut body, id::DOC_TYPE_READ_VERSION, 2);
		element(&mut head, id::EBML, &body);
		// A segment of unknown size: nothing is ever written back.
		put_id(&mut head, id::SEGMENT);
		head.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);

		body.clear();
		uint(&mut body, id::TIMECODE_SCALE, 1_000_000);
		text(&mut body, id::MUXING_APP, "Voelin Studio");
		text(&mut body, id::WRITING_APP, "Voelin Studio");
		// Reserved: the length is only known when the recording stops.
		put_id(&mut body, id::DURATION);
		put_size(&mut body, 8);
		let duration_in_info = body.len();
		body.extend_from_slice(&0f64.to_be_bytes());
		element(&mut head, id::INFO, &body);
		let duration_at = (head.len() - body.len() + duration_in_info) as u64;

		body.clear();
		for (i, track) in tracks.iter().enumerate() {
			let mut entry = Vec::new();
			let number = i as u64 + 1;
			uint(&mut entry, id::TRACK_NUMBER, number);
			uint(&mut entry, id::TRACK_UID, number);
			uint(
				&mut entry,
				id::TRACK_TYPE,
				match track.kind {
					TrackKind::Video { .. } => 1,
					TrackKind::Opus { .. } => 2,
				},
			);
			uint(&mut entry, id::FLAG_LACING, 0);
			text(&mut entry, id::LANGUAGE, "und");
			text(&mut entry, id::CODEC_ID, track.kind.codec_id());
			if !track.private.is_empty() {
				element(&mut entry, id::CODEC_PRIVATE, &track.private);
			}
			match track.kind {
				TrackKind::Video { width, height, .. } => {
					let mut video = Vec::new();
					uint(&mut video, id::PIXEL_WIDTH, u64::from(width.max(1)));
					uint(&mut video, id::PIXEL_HEIGHT, u64::from(height.max(1)));
					element(&mut entry, id::VIDEO, &video);
				}
				TrackKind::Opus { channels } => {
					let mut audio = Vec::new();
					float(&mut audio, id::SAMPLING_FREQUENCY, 48_000.0);
					uint(&mut audio, id::CHANNELS, u64::from(channels.max(1)));
					element(&mut entry, id::AUDIO, &audio);
				}
			}
			element(&mut body, id::TRACK_ENTRY, &entry);
		}
		element(&mut head, id::TRACKS, &body);
		out.write_all(&head)?;
		Ok(Self {
			out,
			duration_at,
			tracks,
			cluster: None,
			buffer: Vec::with_capacity(CLUSTER_BYTES / 4),
			origin: None,
			last_ms: 0,
			bytes: head.len() as u64,
			blocks: 0,
		})
	}

	/// The tracks, in the order their numbers follow.
	pub fn tracks(&self) -> &[Track] {
		&self.tracks
	}

	/// Bytes written so far.
	pub fn bytes(&self) -> u64 {
		self.bytes + self.buffer.len() as u64
	}

	pub fn blocks(&self) -> u64 {
		self.blocks
	}

	/// Length of the recording so far.
	pub fn duration(&self) -> std::time::Duration {
		std::time::Duration::from_millis(self.last_ms)
	}

	/// Add one frame of `track` (0-based) at `ms` from the start of the
	/// stream. The first frame decides where the file's clock starts.
	pub fn write(&mut self, track: usize, ms: u64, keyframe: bool, data: &[u8]) -> Result<()> {
		if track >= self.tracks.len() {
			return Err(Error::InvalidFrame(format!("no track {track} in this recording")));
		}
		if data.is_empty() {
			return Ok(());
		}
		let origin = *self.origin.get_or_insert(ms);
		let ms = ms.saturating_sub(origin);
		self.last_ms = self.last_ms.max(ms);
		let start = match self.cluster {
			Some(start)
				if !(keyframe && track == 0)
					&& self.buffer.len() < CLUSTER_BYTES
					&& ms.saturating_sub(start) < CLUSTER_MS =>
			{
				start
			}
			_ => {
				self.flush_cluster()?;
				self.cluster = Some(ms);
				let mut head = Vec::new();
				uint(&mut head, id::TIMECODE, ms);
				self.buffer.extend_from_slice(&head);
				ms
			}
		};
		let offset = i64::try_from(ms).unwrap_or(i64::MAX) - i64::try_from(start).unwrap_or(0);
		let offset = offset.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
		put_id(&mut self.buffer, id::SIMPLE_BLOCK);
		put_size(&mut self.buffer, data.len() as u64 + 4);
		// Track number as a one-byte EBML varint (at most 127 tracks).
		self.buffer.push(0x80 | (track as u8 + 1));
		self.buffer.extend_from_slice(&offset.to_be_bytes());
		self.buffer.push(if keyframe { 0x80 } else { 0 });
		self.buffer.extend_from_slice(data);
		self.blocks += 1;
		Ok(())
	}

	fn flush_cluster(&mut self) -> Result<()> {
		if self.cluster.take().is_none() || self.buffer.is_empty() {
			self.buffer.clear();
			return Ok(());
		}
		let mut head = Vec::new();
		put_id(&mut head, id::CLUSTER);
		put_size(&mut head, self.buffer.len() as u64);
		self.out.write_all(&head)?;
		self.out.write_all(&self.buffer)?;
		self.bytes += (head.len() + self.buffer.len()) as u64;
		self.buffer.clear();
		Ok(())
	}

	/// Write out the open cluster, fill in the duration and return the sink.
	pub fn finish(mut self) -> Result<W> {
		self.flush_cluster()?;
		let end = self.out.stream_position()?;
		self.out.seek(SeekFrom::Start(self.duration_at))?;
		self.out.write_all(&(self.last_ms as f64).to_be_bytes())?;
		self.out.seek(SeekFrom::Start(end))?;
		self.out.flush()?;
		Ok(self.out)
	}
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use super::*;

	fn varint(size: u64) -> Vec<u8> {
		let mut out = Vec::new();
		put_size(&mut out, size);
		out
	}

	#[test]
	fn sizes_take_the_shortest_form() {
		assert_eq!(varint(0), vec![0x80]);
		assert_eq!(varint(1), vec![0x81]);
		// 127 is the reserved "unknown" value of a one-byte size.
		assert_eq!(varint(126), vec![0xFE]);
		assert_eq!(varint(127), vec![0x40, 0x7F]);
		assert_eq!(varint(0x3FFE), vec![0x7F, 0xFE]);
		assert_eq!(varint(0x3FFF), vec![0x20, 0x3F, 0xFF], "the two-byte all-ones is reserved");
		let mut out = Vec::new();
		uint(&mut out, 0x83, 1);
		assert_eq!(out, vec![0x83, 0x81, 1]);
		out.clear();
		uint(&mut out, 0x2AD7B1, 1_000_000);
		assert_eq!(out, vec![0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]);
	}

	#[test]
	fn annex_b_is_split_and_converted() {
		// SPS, PPS and an IDR, with three- and four-byte start codes.
		let sps = [0x67u8, 0x42, 0x00, 0x1E, 0xAA];
		let pps = [0x68u8, 0xCE, 0x3C, 0x80];
		let idr = [0x65u8, 0x88, 0x84];
		let mut stream = vec![0, 0, 0, 1];
		stream.extend_from_slice(&sps);
		stream.extend_from_slice(&[0, 0, 1]);
		stream.extend_from_slice(&pps);
		stream.extend_from_slice(&[0, 0, 0, 1]);
		stream.extend_from_slice(&idr);
		let units: Vec<&[u8]> = annex_b_units(&stream).collect();
		assert_eq!(units, vec![&sps[..], &pps[..], &idr[..]]);

		let record = avcc(&stream).expect("parameter sets");
		assert_eq!(&record[..6], &[1, 0x42, 0x00, 0x1E, 0xFF, 0xE1]);
		assert_eq!(&record[6..8], &(sps.len() as u16).to_be_bytes());
		assert_eq!(&record[8..8 + sps.len()], &sps);
		assert_eq!(record[8 + sps.len()], 1, "one PPS");

		let mut prefixed = Vec::new();
		to_length_prefixed(&stream, &mut prefixed);
		assert_eq!(&prefixed[..4], &(sps.len() as u32).to_be_bytes());
		assert_eq!(prefixed.len(), 12 + sps.len() + pps.len() + idr.len());
		// A frame without parameter sets has no record.
		assert!(avcc(&[0, 0, 1, 0x65, 1, 2]).is_none());
	}

	#[test]
	fn a_file_has_a_header_tracks_and_clusters() {
		let tracks = vec![
			Track {
				kind: TrackKind::Video { codec: Codec::Vp8, width: 320, height: 240 },
				private: Vec::new(),
			},
			Track { kind: TrackKind::Opus { channels: 2 }, private: opus_head(2) },
		];
		let mut writer = Writer::new(Cursor::new(Vec::new()), true, tracks).unwrap();
		writer.write(0, 1000, true, &[1, 2, 3]).unwrap();
		writer.write(1, 1000, true, &[4, 5]).unwrap();
		writer.write(0, 1033, false, &[6]).unwrap();
		// A video keyframe opens a new cluster.
		writer.write(0, 1066, true, &[7]).unwrap();
		assert_eq!(writer.blocks(), 4);
		assert_eq!(writer.duration(), std::time::Duration::from_millis(66));
		let file = writer.finish().unwrap().into_inner();
		// EBML header, "webm", two clusters.
		assert_eq!(&file[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
		assert!(file.windows(4).any(|w| w == b"webm"));
		assert!(file.windows(5).any(|w| w == b"V_VP8"));
		assert!(file.windows(6).any(|w| w == b"A_OPUS"));
		assert!(file.windows(8).any(|w| w == b"OpusHead"));
		let clusters = file.windows(4).filter(|w| *w == [0x1F, 0x43, 0xB6, 0x75]).count();
		assert_eq!(clusters, 2, "a keyframe starts a cluster");

		// WebM refuses H.264; Matroska takes it.
		let h264 = vec![Track {
			kind: TrackKind::Video { codec: Codec::H264, width: 16, height: 16 },
			private: vec![1, 2, 3],
		}];
		assert!(Writer::new(Cursor::new(Vec::new()), true, h264.clone()).is_err());
		let mut writer = Writer::new(Cursor::new(Vec::new()), false, h264).unwrap();
		writer.write(0, 0, true, &[9]).unwrap();
		assert!(writer.write(1, 0, true, &[9]).is_err(), "no track 1");
		let file = writer.finish().unwrap().into_inner();
		assert!(file.windows(8).any(|w| w == b"matroska"));
		assert!(file.windows(15).any(|w| w == b"V_MPEG4/ISO/AVC"));
		assert!(Writer::new(Cursor::new(Vec::new()), false, Vec::new()).is_err());
	}
}
