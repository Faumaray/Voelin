//! The replay buffer: the last few seconds, kept encoded.
//!
//! Every packet is kept until it falls out of the window
//! (`studio.replay_seconds`, no maximum), and the window always reaches back
//! to a keyframe, so [`ReplayBuffer::save_clip`] can write a playable file
//! without re-encoding anything.
//!
//! Long windows do not have to fit in memory: past
//! `studio.replay_memory_mb` the oldest packets are moved to a temporary
//! file and only their offsets stay in memory. The file is emptied again
//! whenever every packet in it has aged out, so a buffer that briefly
//! overflowed does not keep the disk space.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::debug;

use crate::codec::Codec;
use crate::studio::output::record::Recorder;
use crate::studio::output::{OutputSink, Packet, Track};
use crate::{Error, Result};

/// Where a kept packet's bytes are.
enum Data {
	Memory(Vec<u8>),
	Spilled { offset: u64, len: u32 },
}

/// One kept packet.
struct Entry {
	track: Track,
	pts_90khz: u64,
	keyframe: bool,
	width: u32,
	height: u32,
	data: Data,
}

/// The temporary file holding packets that no longer fit in memory.
struct Spill {
	path: PathBuf,
	file: File,
	/// Bytes written so far (the next offset).
	end: u64,
	/// Packets still in it.
	entries: usize,
}

impl Drop for Spill {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.path);
	}
}

/// What the buffer holds right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
	pub packets: u64,
	/// How far back the buffer reaches.
	pub duration: Duration,
	pub memory_bytes: u64,
	pub spilled_bytes: u64,
	/// Clips written.
	pub clips: u64,
}

/// The rolling buffer of encoded packets; see the [module docs](self).
pub struct ReplayBuffer {
	length: Duration,
	memory_limit: u64,
	layer: u32,
	channels: u16,
	entries: VecDeque<Entry>,
	memory: u64,
	spill: Option<Spill>,
	/// Where the temporary file goes.
	spill_dir: PathBuf,
	codec: Option<Codec>,
	clips: u64,
	needs_keyframe: bool,
}

impl ReplayBuffer {
	/// A buffer holding `length` of `layer` (0 turns it off) with at most
	/// `memory_mb` megabytes in memory before it spills to disk.
	pub fn new(length: Duration, memory_mb: u64, layer: u32, channels: u16) -> Self {
		Self {
			length,
			memory_limit: memory_mb.saturating_mul(1 << 20),
			layer,
			channels: channels.clamp(1, 8),
			entries: VecDeque::new(),
			memory: 0,
			spill: None,
			spill_dir: std::env::temp_dir(),
			codec: None,
			clips: 0,
			needs_keyframe: true,
		}
	}

	/// Where the spill file goes (the system's temporary directory by
	/// default).
	pub fn with_spill_dir(mut self, dir: impl Into<PathBuf>) -> Self {
		self.spill_dir = dir.into();
		self
	}

	/// How far back the buffer keeps packets; 0 turns it off and frees what
	/// it holds. There is no maximum.
	pub fn set_length(&mut self, length: Duration) {
		self.length = length;
		if length.is_zero() {
			self.clear();
		} else {
			self.trim();
		}
	}

	pub fn length(&self) -> Duration {
		self.length
	}

	/// Megabytes kept in memory before older packets go to disk.
	pub fn set_memory_mb(&mut self, memory_mb: u64) {
		self.memory_limit = memory_mb.saturating_mul(1 << 20);
		let _ = self.spill_over_limit();
	}

	pub fn is_on(&self) -> bool {
		!self.length.is_zero()
	}

	pub fn clear(&mut self) {
		self.entries.clear();
		self.memory = 0;
		self.spill = None;
		self.needs_keyframe = true;
	}

	pub fn stats(&self) -> ReplayStats {
		let first = self.entries.front().map_or(0, |e| e.pts_90khz);
		let last = self.entries.back().map_or(0, |e| e.pts_90khz);
		ReplayStats {
			packets: self.entries.len() as u64,
			duration: Duration::from_micros(last.saturating_sub(first) * 1_000_000 / 90_000),
			memory_bytes: self.memory,
			spilled_bytes: self.spill.as_ref().map_or(0, |s| s.end),
			clips: self.clips,
		}
	}

	/// Drop packets older than the window, never past the keyframe the
	/// window starts in.
	fn trim(&mut self) {
		let Some(newest) = self.entries.back().map(|e| e.pts_90khz) else { return };
		let window = (self.length.as_micros() as u64).saturating_mul(90_000) / 1_000_000;
		let cutoff = newest.saturating_sub(window);
		// The last video keyframe at or before the cutoff: everything before
		// it can go.
		let keep = self
			.entries
			.iter()
			.enumerate()
			.rfind(|(_, e)| e.keyframe && e.track.is_video() && e.pts_90khz <= cutoff)
			.map(|(i, _)| i);
		let Some(keep) = keep else { return };
		let dropped: Vec<Data> = self.entries.drain(..keep).map(|e| e.data).collect();
		for data in &dropped {
			self.release(data);
		}
		self.reset_spill();
	}

	/// Account for an entry that is gone.
	fn release(&mut self, data: &Data) {
		match data {
			Data::Memory(bytes) => self.memory = self.memory.saturating_sub(bytes.len() as u64),
			Data::Spilled { .. } => {
				if let Some(spill) = &mut self.spill {
					spill.entries -= 1;
				}
			}
		}
	}

	/// Throw the spill file away once nothing in it is kept any more.
	fn reset_spill(&mut self) {
		if self.spill.as_ref().is_some_and(|s| s.entries == 0) {
			self.spill = None;
		}
	}

	/// Move the oldest in-memory packets to disk until memory is within the
	/// limit.
	fn spill_over_limit(&mut self) -> Result<()> {
		if self.memory <= self.memory_limit {
			return Ok(());
		}
		for index in 0..self.entries.len() {
			if self.memory <= self.memory_limit {
				break;
			}
			let Data::Memory(bytes) = &self.entries[index].data else { continue };
			let len = bytes.len();
			let spill = match &mut self.spill {
				Some(spill) => spill,
				None => {
					let path = self.spill_dir.join(format!(
						"voelin-replay-{}-{:?}.bin",
						std::process::id(),
						std::thread::current().id()
					));
					let file = File::options()
						.create(true)
						.truncate(true)
						.read(true)
						.write(true)
						.open(&path)?;
					debug!(path = %path.display(), "replay buffer spilling to disk");
					self.spill.insert(Spill { path, file, end: 0, entries: 0 })
				}
			};
			let offset = spill.end;
			// Take the bytes out before writing, so a failed write drops the
			// packet rather than leaving it counted twice.
			let Data::Memory(bytes) = std::mem::replace(
				&mut self.entries[index].data,
				Data::Spilled { offset, len: len as u32 },
			) else {
				unreachable!("checked above")
			};
			spill.file.seek(SeekFrom::Start(offset))?;
			spill.file.write_all(&bytes)?;
			spill.end += len as u64;
			spill.entries += 1;
			self.memory -= len as u64;
		}
		Ok(())
	}

	/// The bytes of one kept packet, into `out`.
	fn read(&mut self, index: usize, out: &mut Vec<u8>) -> Result<()> {
		out.clear();
		match &self.entries[index].data {
			Data::Memory(bytes) => out.extend_from_slice(bytes),
			Data::Spilled { offset, len } => {
				let (offset, len) = (*offset, *len as usize);
				let Some(spill) = &mut self.spill else {
					return Err(Error::InvalidFrame("the replay spill file is gone".into()));
				};
				out.resize(len, 0);
				spill.file.seek(SeekFrom::Start(offset))?;
				spill.file.read_exact(out)?;
			}
		}
		Ok(())
	}

	/// Write everything the buffer holds, from its first keyframe, to
	/// `path`; the container comes from the file name. Returns how long the
	/// clip is.
	///
	/// The buffer keeps its packets, so several clips can be saved from one
	/// window.
	pub fn save_clip(&mut self, path: impl AsRef<Path>) -> Result<Duration> {
		let path = path.as_ref();
		if !self.is_on() {
			return Err(Error::InvalidFrame(
				"the replay buffer is off (studio.replay_seconds is 0)".into(),
			));
		}
		let start =
			self.entries.iter().position(|e| e.keyframe && e.track.is_video()).ok_or_else(
				|| Error::InvalidFrame("the replay buffer holds no keyframe yet".into()),
			)?;
		let mut recorder = Recorder::start(path, self.layer, self.channels)?;
		let mut bytes = Vec::new();
		for index in start..self.entries.len() {
			self.read(index, &mut bytes)?;
			let entry = &self.entries[index];
			recorder.write(&Packet {
				track: entry.track,
				pts_90khz: entry.pts_90khz,
				keyframe: entry.keyframe,
				width: entry.width,
				height: entry.height,
				data: &bytes,
			})?;
		}
		let length = recorder.duration();
		recorder.finish()?;
		self.clips += 1;
		debug!(path = %path.display(), ?length, "replay clip saved");
		Ok(length)
	}
}

impl OutputSink for ReplayBuffer {
	fn name(&self) -> &str {
		"replay"
	}

	fn wants(&self, track: Track) -> bool {
		match track {
			Track::Video { layer, .. } => layer == self.layer,
			Track::Audio { .. } => true,
		}
	}

	fn needs_keyframe(&mut self) -> bool {
		self.is_on() && self.needs_keyframe
	}

	fn write(&mut self, packet: &Packet<'_>) -> Result<()> {
		if !self.is_on() || !self.wants(packet.track) {
			return Ok(());
		}
		if let Track::Video { codec, .. } = packet.track {
			// A codec change makes everything before it unplayable.
			if self.codec.is_some_and(|c| c != codec) {
				self.clear();
			}
			self.codec = Some(codec);
			if packet.keyframe {
				self.needs_keyframe = false;
			}
		}
		// Nothing before the first keyframe: a clip has to start at one.
		if self.entries.is_empty() && !(packet.keyframe && packet.track.is_video()) {
			return Ok(());
		}
		self.entries.push_back(Entry {
			track: packet.track,
			pts_90khz: packet.pts_90khz,
			keyframe: packet.keyframe,
			width: packet.width,
			height: packet.height,
			data: Data::Memory(packet.data.to_vec()),
		});
		self.memory += packet.data.len() as u64;
		self.trim();
		self.spill_over_limit()
	}

	fn bytes(&self) -> u64 {
		self.memory + self.spill.as_ref().map_or(0, |s| s.end)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn video(ms: u64, keyframe: bool, len: usize) -> (Vec<u8>, u64, bool) {
		(vec![(ms % 251) as u8; len], ms, keyframe)
	}

	fn push(buffer: &mut ReplayBuffer, ms: u64, keyframe: bool, len: usize) {
		let (data, ms, keyframe) = video(ms, keyframe, len);
		buffer
			.write(&Packet {
				track: Track::Video { codec: Codec::Vp8, layer: 0 },
				pts_90khz: ms * 90,
				keyframe,
				width: 320,
				height: 240,
				data: &data,
			})
			.unwrap();
	}

	fn dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"voelin-replay-{name}-{}-{:?}",
			std::process::id(),
			std::thread::current().id()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn it_starts_at_a_keyframe_and_keeps_the_window() {
		let mut buffer = ReplayBuffer::new(Duration::from_secs(2), 64, 0, 2);
		assert!(buffer.is_on() && buffer.needs_keyframe());
		// Nothing before the first keyframe.
		for ms in [0, 33, 66] {
			push(&mut buffer, ms, false, 10);
		}
		assert_eq!(buffer.stats().packets, 0);
		push(&mut buffer, 100, true, 100);
		assert!(!buffer.needs_keyframe());
		// Five seconds at 30 fps with a keyframe every second.
		for n in 1..150u64 {
			let ms = 100 + n * 33;
			push(&mut buffer, ms, n.is_multiple_of(30), 10);
		}
		let stats = buffer.stats();
		// The window reaches back to the keyframe that covers it: at most a
		// keyframe interval more than the two seconds asked for.
		assert!(
			stats.duration >= Duration::from_secs(2) && stats.duration <= Duration::from_secs(3),
			"{stats:?}"
		);
		assert_eq!(stats.spilled_bytes, 0, "everything fits in memory");
	}

	#[test]
	fn it_spills_to_disk_and_comes_back() {
		let dir = dir("spill");
		// A tiny memory limit: almost everything goes to the file.
		let mut buffer = ReplayBuffer::new(Duration::from_secs(4), 0, 0, 2).with_spill_dir(&dir);
		push(&mut buffer, 0, true, 4096);
		for n in 1..120u64 {
			push(&mut buffer, n * 33, n.is_multiple_of(30), 4096);
		}
		let stats = buffer.stats();
		assert!(stats.spilled_bytes > 0, "{stats:?}");
		assert!(stats.memory_bytes < stats.spilled_bytes, "{stats:?}");
		assert_eq!(buffer.bytes(), stats.memory_bytes + stats.spilled_bytes);

		// The clip reads the spilled packets back.
		let clip = dir.join("clip.webm");
		let length = buffer.save_clip(&clip).unwrap();
		assert!(length >= Duration::from_secs(3), "{length:?}");
		let written = std::fs::read(&clip).unwrap();
		assert!(written.len() > 4096 * 50, "{} bytes", written.len());
		assert!(written.windows(4).any(|w| w == b"webm"));
		// The buffer still holds everything.
		assert_eq!(buffer.stats().packets, stats.packets);
		assert_eq!(buffer.stats().clips, 1);

		// Turning it off frees everything, including the file.
		buffer.set_length(Duration::ZERO);
		assert_eq!(buffer.stats(), ReplayStats { clips: 1, ..ReplayStats::default() });
		assert!(buffer.save_clip(&clip).is_err(), "a buffer that is off saves nothing");
		std::fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn a_window_that_shrinks_drops_the_old_packets() {
		let mut buffer = ReplayBuffer::new(Duration::from_secs(10), 64, 0, 2);
		push(&mut buffer, 0, true, 100);
		for n in 1..300u64 {
			push(&mut buffer, n * 33, n.is_multiple_of(30), 100);
		}
		let before = buffer.stats().packets;
		buffer.set_length(Duration::from_secs(1));
		let after = buffer.stats().packets;
		assert!(after < before / 4, "{before} -> {after}");
		assert!(buffer.stats().duration <= Duration::from_secs(2));
		// The first packet kept is still a keyframe.
		assert!(buffer.entries.front().is_some_and(|e| e.keyframe));
	}

	#[test]
	fn a_codec_change_starts_over() {
		let mut buffer = ReplayBuffer::new(Duration::from_secs(5), 64, 0, 2);
		push(&mut buffer, 0, true, 100);
		push(&mut buffer, 33, false, 100);
		assert_eq!(buffer.stats().packets, 2);
		buffer
			.write(&Packet {
				track: Track::Video { codec: Codec::Vp9, layer: 0 },
				pts_90khz: 66 * 90,
				keyframe: true,
				width: 320,
				height: 240,
				data: &[1, 2, 3],
			})
			.unwrap();
		assert_eq!(buffer.stats().packets, 1, "the VP8 packets were dropped");
	}

	#[test]
	fn other_layers_are_left_alone() {
		let buffer = ReplayBuffer::new(Duration::from_secs(1), 64, 1, 2);
		assert!(!buffer.wants(Track::Video { codec: Codec::Vp8, layer: 0 }));
		assert!(buffer.wants(Track::Video { codec: Codec::Vp8, layer: 1 }));
		assert!(buffer.wants(Track::Audio { channels: 2 }));
		assert_eq!(buffer.name(), "replay");
	}
}
