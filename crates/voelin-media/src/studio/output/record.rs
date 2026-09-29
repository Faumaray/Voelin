//! Recording the studio to a file, without re-encoding.
//!
//! The recorder waits for the first keyframe of the layer it records (a file
//! that starts mid-picture is useless), then writes an [`ebml`] file and
//! every packet after it. The audio track is declared from the start, so
//! audio that arrives before the first keyframe is simply dropped rather
//! than shifting the file's clock.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::debug;

use crate::codec::Codec;
use crate::studio::output::ebml::{self, Track as MkvTrack, TrackKind, Writer};
use crate::studio::output::{OutputSink, Packet, Track};
use crate::{Error, Result};

/// The container a recording is written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
	/// VP8, VP9, AV1 and Opus.
	#[default]
	WebM,
	/// Also H.264 and H.265.
	Matroska,
}

impl Format {
	/// The format a file name asks for: `.webm`, or `.mkv` / `.mka`.
	pub fn of_path(path: &Path) -> Result<Self> {
		match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
			Some("webm") => Ok(Self::WebM),
			Some("mkv" | "mka" | "matroska") => Ok(Self::Matroska),
			_ => Err(Error::InvalidFrame(format!(
				"cannot tell the recording format of {}: use .webm or .mkv",
				path.display()
			))),
		}
	}

	/// The format that can hold `codec`, preferring what `wanted` asks for.
	pub fn for_codec(wanted: Self, codec: Codec) -> Self {
		match codec {
			Codec::Vp8 | Codec::Vp9 | Codec::Av1 => wanted,
			// WebM has no place for these.
			Codec::H264 | Codec::H265 => Self::Matroska,
		}
	}
}

/// Writes the studio's packets to a file; see the [module docs](self).
pub struct Recorder {
	path: PathBuf,
	name: String,
	format: Format,
	/// The layer recorded; other layers are ignored.
	layer: u32,
	channels: u16,
	writer: Option<Writer<BufWriter<File>>>,
	/// Set until the first keyframe opened the file.
	waiting: bool,
	/// H.264 frames are rewritten length-prefixed.
	scratch: Vec<u8>,
	video: Option<Codec>,
}

impl Recorder {
	/// Start recording `layer` to `path`. The container comes from the file
	/// name; a codec WebM cannot hold moves the recording to Matroska (and
	/// the file keeps the name it was given).
	pub fn start(path: impl Into<PathBuf>, layer: u32, channels: u16) -> Result<Self> {
		let path = path.into();
		let format = Format::of_path(&path)?;
		if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
			std::fs::create_dir_all(parent)?;
		}
		// Fail now rather than at the first keyframe if the path is bad.
		File::create(&path)?;
		let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
		Ok(Self {
			path,
			name,
			format,
			layer,
			channels: channels.clamp(1, 8),
			writer: None,
			waiting: true,
			scratch: Vec::new(),
			video: None,
		})
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// What the file actually holds (a codec WebM cannot hold moved it).
	pub fn format(&self) -> Format {
		self.format
	}

	/// Whether the first keyframe arrived and the file is being written.
	pub fn started(&self) -> bool {
		self.writer.is_some()
	}

	/// Length of the recording so far.
	pub fn duration(&self) -> Duration {
		self.writer.as_ref().map_or(Duration::ZERO, Writer::duration)
	}

	/// Open the file on the first keyframe of the recorded layer.
	fn open(&mut self, packet: &Packet<'_>, codec: Codec) -> Result<()> {
		let private = if codec == Codec::H264 {
			ebml::avcc(packet.data).ok_or_else(|| Error::Encoder {
				codec,
				message: "the keyframe carries no parameter sets to record".into(),
			})?
		} else {
			Vec::new()
		};
		self.format = Format::for_codec(self.format, codec);
		let tracks = vec![
			MkvTrack {
				kind: TrackKind::Video {
					codec,
					width: packet.width.max(2),
					height: packet.height.max(2),
				},
				private,
			},
			MkvTrack {
				kind: TrackKind::Opus { channels: self.channels },
				private: ebml::opus_head(self.channels),
			},
		];
		let file = BufWriter::new(File::create(&self.path)?);
		let writer = Writer::new(file, self.format == Format::WebM, tracks)?;
		debug!(path = %self.path.display(), %codec, format = ?self.format, "recording");
		self.writer = Some(writer);
		self.waiting = false;
		self.video = Some(codec);
		Ok(())
	}
}

impl OutputSink for Recorder {
	fn name(&self) -> &str {
		&self.name
	}

	fn wants(&self, track: Track) -> bool {
		match track {
			Track::Video { layer, .. } => layer == self.layer,
			Track::Audio { .. } => true,
		}
	}

	fn needs_keyframe(&mut self) -> bool {
		self.waiting
	}

	fn write(&mut self, packet: &Packet<'_>) -> Result<()> {
		let (index, codec) = match packet.track {
			Track::Video { codec, layer } if layer == self.layer => (0, Some(codec)),
			Track::Video { .. } => return Ok(()),
			Track::Audio { .. } => (1, None),
		};
		if self.writer.is_none() {
			// Nothing before the first keyframe of the recorded layer.
			let Some(codec) = codec.filter(|_| packet.keyframe) else { return Ok(()) };
			self.open(packet, codec)?;
		}
		let Some(writer) = &mut self.writer else { return Ok(()) };
		// A codec change mid-recording would need a new file.
		if let Some(codec) = codec
			&& self.video != Some(codec)
		{
			return Err(Error::Encoder {
				codec,
				message: "the codec changed while recording; stop and start again".into(),
			});
		}
		if codec == Some(Codec::H264) {
			ebml::to_length_prefixed(packet.data, &mut self.scratch);
			let scratch = std::mem::take(&mut self.scratch);
			let result = writer.write(index, packet.ms(), packet.keyframe, &scratch);
			self.scratch = scratch;
			return result;
		}
		writer.write(index, packet.ms(), packet.keyframe, packet.data)
	}

	fn finish(&mut self) -> Result<()> {
		let Some(writer) = self.writer.take() else { return Ok(()) };
		let blocks = writer.blocks();
		let file = writer.finish()?;
		drop(file);
		debug!(path = %self.path.display(), blocks, "recording finished");
		Ok(())
	}

	fn bytes(&self) -> u64 {
		self.writer.as_ref().map_or(0, Writer::bytes)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"voelin-studio-{name}-{}-{:?}",
			std::process::id(),
			std::thread::current().id()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn video(layer: u32, ms: u64, keyframe: bool, data: &[u8]) -> Packet<'_> {
		Packet {
			track: Track::Video { codec: Codec::Vp8, layer },
			pts_90khz: ms * 90,
			keyframe,
			width: 320,
			height: 240,
			data,
		}
	}

	#[test]
	fn formats_come_from_the_name_and_the_codec() {
		assert_eq!(Format::of_path(Path::new("a.webm")).unwrap(), Format::WebM);
		assert_eq!(Format::of_path(Path::new("a.MKV")).unwrap(), Format::Matroska);
		assert!(Format::of_path(Path::new("a.mp4")).is_err());
		assert!(Format::of_path(Path::new("a")).is_err());
		assert_eq!(Format::for_codec(Format::WebM, Codec::Vp9), Format::WebM);
		assert_eq!(Format::for_codec(Format::WebM, Codec::H264), Format::Matroska);
	}

	#[test]
	fn recording_starts_at_a_keyframe() {
		let dir = dir("record");
		let path = dir.join("clip.webm");
		let mut recorder = Recorder::start(&path, 0, 2).unwrap();
		assert!(recorder.needs_keyframe());
		// Before the first keyframe nothing is written.
		recorder.write(&video(0, 0, false, &[1, 2])).unwrap();
		recorder
			.write(&Packet { track: Track::Audio { channels: 2 }, ..video(0, 0, true, &[9]) })
			.unwrap();
		assert!(!recorder.started());
		// Another layer is ignored even when it is a keyframe.
		assert!(!recorder.wants(Track::Video { codec: Codec::Vp8, layer: 1 }));
		recorder.write(&video(1, 10, true, &[3])).unwrap();
		assert!(!recorder.started());

		recorder.write(&video(0, 100, true, &[1, 2, 3])).unwrap();
		assert!(recorder.started() && !recorder.needs_keyframe());
		for ms in (133..1000).step_by(33) {
			recorder.write(&video(0, ms, false, &[4, 5])).unwrap();
			recorder
				.write(&Packet {
					track: Track::Audio { channels: 2 },
					pts_90khz: ms * 90,
					keyframe: true,
					width: 0,
					height: 0,
					data: &[7, 7, 7],
				})
				.unwrap();
		}
		// The file's clock starts at the first keyframe, not at zero.
		assert!(recorder.duration() >= Duration::from_millis(800));
		recorder.finish().unwrap();
		let written = std::fs::read(&path).unwrap();
		assert!(written.len() > 100, "{} bytes", written.len());
		assert!(written.windows(4).any(|w| w == b"webm"));
		std::fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn a_codec_change_stops_the_recording() {
		let dir = dir("codec-change");
		let path = dir.join("clip.mkv");
		let mut recorder = Recorder::start(&path, 0, 2).unwrap();
		recorder.write(&video(0, 0, true, &[1])).unwrap();
		let mut other = video(0, 33, false, &[2]);
		other.track = Track::Video { codec: Codec::Vp9, layer: 0 };
		assert!(recorder.write(&other).is_err());
		recorder.finish().unwrap();
		std::fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn an_unwritable_path_fails_at_once() {
		assert!(Recorder::start("/nonexistent-root-dir/x/clip.webm", 0, 2).is_err());
		assert!(Recorder::start("clip.mp4", 0, 2).is_err());
	}
}
