//! RTMP and RTMPS: pushing the studio to a broadcast service (Twitch,
//! YouTube, an nginx-rtmp, anything that takes what OBS sends).
//!
//! The connection is libavformat's, loaded at runtime with the rest of
//! FFmpeg ([`crate::ffmpeg::avio`]): its `rtmp://` and `rtmps://` protocols
//! do the handshake, `connect`, `publish` and TLS, and take an FLV byte
//! stream. The FLV is ours ([`flv`]), as the recordings' Matroska is, so no
//! libavformat struct is touched; the one field read is where a failed
//! write lands, found at load.
//!
//! - Video: the studio's H.264 packets of one layer, as they are (no second
//!   encode). Another stream codec is refused with the reason: classic RTMP
//!   carries H.264 only.
//! - Audio: the studio's Opus, decoded and encoded as AAC-LC by FFmpeg's own
//!   codecs ([`crate::ffmpeg::audio`]) on the output's thread.
//! - The stream key: in the URL (`rtmp://host/app/key`) or given apart (the
//!   output's token), then sent as the play path with the URL's path as the
//!   application. The output's name never shows the key.
//!
//! A thread of its own runs the connection, fed through a bounded queue, so
//! a slow upload never holds up an encoder: a full queue drops the packet,
//! and video resumes at the next keyframe (which it asks for). A connection
//! that fails is made again, after 1, 2, 4, ... up to 30 seconds, for as
//! long as the output exists, each new one starting at a keyframe; the
//! first connection is made by [`Rtmp::start`], so a wrong address or key
//! fails right there. [`OutputSink::finish`] (End Stream) unpublishes and
//! closes, cutting a dead network off after a few seconds.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::codec::Codec;
use crate::studio::output::{OutputSink, Packet, Track};
use crate::{Error, Result};

/// Packets waiting for the connection's thread: about two seconds of a
/// 60 fps stream with its audio.
const QUEUE: usize = 256;
/// The AAC bitrate (bit/s): what the big services recommend.
pub const AAC_BITRATE: u64 = 160_000;
/// How long a connection may wait for the network (connecting, a send)
/// before it is given up and made again.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Waits between connection attempts: doubling from the first to the last.
const BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(30));
/// How long closing may take (unpublish) before the network is cut off.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// Audio kept while waiting for the first keyframe (as a recording does).
const BACKLOG_MS: i64 = 2_000;
/// Audio waits for the video of its time at most this long (it is written in
/// time order with the video, and the video comes later: the encoders' delay).
const INTERLEAVE_MS: i64 = 1_000;

fn error(message: impl Into<String>) -> Error {
	Error::Output { output: "RTMP", message: message.into() }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why `codec` cannot go out over RTMP.
fn unsupported(codec: Codec) -> Error {
	error(format!(
		"RTMP carries H.264 video; the stream encodes {codec}. Choose H.264 as the stream codec \
		 for this output"
	))
}

/// Where to connect: the URL libavformat opens, its protocol options and a
/// name for the UI that does not show the stream key.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
	url: String,
	options: Vec<(&'static str, String)>,
	name: String,
}

impl Target {
	fn new(url: &str, key: Option<&str>) -> Result<Self> {
		let url = url.trim();
		let lower = url.to_ascii_lowercase();
		let rest = ["rtmp://", "rtmps://"]
			.iter()
			.find_map(|scheme| lower.starts_with(scheme).then(|| &url[scheme.len()..]))
			.ok_or_else(|| error(format!("not an RTMP address: {url}")))?;
		let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
		if host.is_empty() {
			return Err(error(format!("no server in {url}")));
		}
		let mut options = vec![
			("rw_timeout", TIMEOUT.as_micros().to_string()),
			// Small tags (audio) go out at once.
			("tcp_nodelay", "1".to_owned()),
		];
		let name = match key.map(str::trim).filter(|k| !k.is_empty()) {
			Some(key) => {
				let app = path.trim_matches('/');
				if !app.is_empty() {
					options.push(("rtmp_app", app.to_owned()));
				}
				options.push(("rtmp_playpath", key.to_owned()));
				url.trim_end_matches('/').to_owned()
			}
			// The key is the last part of the path.
			None => match url.trim_end_matches('/').rsplit_once('/') {
				Some((base, _)) if path.trim_matches('/').contains('/') => format!("{base}/…"),
				_ => url.to_owned(),
			},
		};
		Ok(Self { url: url.to_owned(), options, name })
	}
}

/// One packet on its way to the connection's thread.
struct Queued {
	video: bool,
	keyframe: bool,
	pts_90khz: u64,
	width: u32,
	height: u32,
	data: Vec<u8>,
}

/// What an RTMP output is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RtmpStats {
	/// Connected and publishing.
	pub connected: bool,
	/// Bytes sent (FLV), over all connections.
	pub bytes: u64,
	/// FLV tags sent.
	pub tags: u64,
	/// Packets dropped because the upload could not keep up, or while
	/// disconnected.
	pub dropped: u64,
	/// Connections made again after one failed.
	pub reconnects: u64,
	/// The last failure, while it is not connected.
	pub error: Option<String>,
}

#[derive(Default)]
struct Shared {
	/// End the connection (cleanly).
	stop: AtomicBool,
	/// Interrupt whatever FFmpeg is waiting for.
	abort: Arc<AtomicBool>,
	/// A keyframe is wanted; taken by [`OutputSink::needs_keyframe`].
	keyframe: AtomicBool,
	connected: AtomicBool,
	bytes: AtomicU64,
	tags: AtomicU64,
	dropped: AtomicU64,
	reconnects: AtomicU64,
	error: Mutex<Option<String>>,
}

/// An RTMP output; see the [module docs](self). Closes when dropped.
pub struct Rtmp {
	name: String,
	layer: u32,
	queue: Option<std::sync::mpsc::SyncSender<Queued>>,
	thread: Option<JoinHandle<()>>,
	shared: Arc<Shared>,
	/// A video packet was dropped: no more video until a keyframe.
	skip_video: bool,
}

impl Rtmp {
	/// Whether `url` is one this output takes.
	pub fn handles(url: &str) -> bool {
		let url = url.trim_start().to_ascii_lowercase();
		url.starts_with("rtmp://") || url.starts_with("rtmps://")
	}

	/// Connect to `url` and push `layer` of the studio and its audio. The
	/// stream key is the last part of the URL's path, or `key` if given.
	/// `codec` is what the studio encodes, if known: anything but H.264 is
	/// refused (also later, when its first packet comes).
	///
	/// Fails when there is no FFmpeg with libavformat, or the first
	/// connection does (a wrong address, a refused key).
	pub async fn start(
		url: &str,
		key: Option<&str>,
		layer: u32,
		codec: Option<Codec>,
	) -> Result<Self> {
		if let Some(codec) = codec.filter(|c| *c != Codec::H264) {
			return Err(unsupported(codec));
		}
		let target = Target::new(url, key)?;
		#[cfg(not(feature = "ffmpeg"))]
		{
			let _ = (target, layer);
			Err(error("this build has no FFmpeg (feature `ffmpeg`), which RTMP goes through"))
		}
		#[cfg(feature = "ffmpeg")]
		{
			session::start(target, layer).await
		}
	}

	pub fn stats(&self) -> RtmpStats {
		let shared = &self.shared;
		let connected = shared.connected.load(Ordering::Relaxed);
		RtmpStats {
			connected,
			bytes: shared.bytes.load(Ordering::Relaxed),
			tags: shared.tags.load(Ordering::Relaxed),
			dropped: shared.dropped.load(Ordering::Relaxed),
			reconnects: shared.reconnects.load(Ordering::Relaxed),
			error: if connected { None } else { lock(&shared.error).clone() },
		}
	}

	/// Unpublish and close; a network that does not answer is cut off
	/// after [`CLOSE_GRACE`].
	fn teardown(&mut self) {
		self.shared.stop.store(true, Ordering::Relaxed);
		self.queue = None;
		let Some(thread) = self.thread.take() else { return };
		let deadline = Instant::now() + CLOSE_GRACE;
		while !thread.is_finished() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(10));
		}
		self.shared.abort.store(true, Ordering::Relaxed);
		let _ = thread.join();
	}
}

impl Drop for Rtmp {
	fn drop(&mut self) {
		self.teardown();
	}
}

impl OutputSink for Rtmp {
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
		self.shared.keyframe.swap(false, Ordering::Relaxed)
	}

	fn write(&mut self, packet: &Packet<'_>) -> Result<()> {
		let Some(queue) = &self.queue else { return Ok(()) };
		if let Track::Video { codec, layer } = packet.track {
			if layer != self.layer {
				return Ok(());
			}
			if codec != Codec::H264 {
				return Err(unsupported(codec));
			}
			if self.skip_video {
				if !packet.keyframe {
					return Ok(());
				}
				self.skip_video = false;
			}
		}
		let queued = Queued {
			video: packet.track.is_video(),
			keyframe: packet.keyframe,
			pts_90khz: packet.pts_90khz,
			width: packet.width,
			height: packet.height,
			data: packet.data.to_vec(),
		};
		match queue.try_send(queued) {
			Ok(()) => Ok(()),
			Err(std::sync::mpsc::TrySendError::Full(queued)) => {
				self.shared.dropped.fetch_add(1, Ordering::Relaxed);
				if queued.video {
					self.skip_video = true;
					self.shared.keyframe.store(true, Ordering::Relaxed);
				}
				Ok(())
			}
			Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(error(
				lock(&self.shared.error).clone().unwrap_or_else(|| "the RTMP output ended".into()),
			)),
		}
	}

	fn finish(&mut self) -> Result<()> {
		self.teardown();
		Ok(())
	}

	fn bytes(&self) -> u64 {
		self.shared.bytes.load(Ordering::Relaxed)
	}

	fn error(&self) -> Option<String> {
		self.stats().error
	}
}

/// The connection's thread.
#[cfg(feature = "ffmpeg")]
mod session {
	use std::collections::VecDeque;
	use std::sync::Arc;
	use std::sync::atomic::Ordering;
	use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
	use std::time::{Duration, Instant};

	use tracing::{debug, warn};

	use super::{
		AAC_BITRATE, BACKLOG_MS, BACKOFF, INTERLEAVE_MS, QUEUE, Queued, Rtmp, Shared, Target,
		error, lock,
	};
	use crate::Result;
	use crate::ffmpeg::audio::{AUDIO_SPECIFIC_CONFIG, AacEncoder, RATE};
	use crate::ffmpeg::avio::Connection;
	use crate::studio::output::{ebml, flv};

	/// How often a thread that waits checks whether to stop.
	const POLL: Duration = Duration::from_millis(100);

	pub(super) async fn start(target: Target, layer: u32) -> Result<Rtmp> {
		Connection::available().map_err(error)?;
		let aac = AacEncoder::new(AAC_BITRATE).map_err(error)?;
		let shared = Arc::new(Shared::default());
		let (queue, packets) = sync_channel(QUEUE);
		let (ready, first) = tokio::sync::oneshot::channel();
		let name = target.name.clone();
		let worker = Worker { target, shared: shared.clone(), packets, aac, buf: Vec::new() };
		let thread = std::thread::Builder::new()
			.name("voelin-rtmp".into())
			.spawn(move || worker.run(ready))
			.map_err(|e| error(e.to_string()))?;
		match first.await {
			Ok(Ok(())) => {}
			Ok(Err(e)) => {
				let _ = thread.join();
				return Err(error(e));
			}
			Err(_) => return Err(error("the RTMP thread ended")),
		}
		debug!(name, "RTMP output connected");
		Ok(Rtmp {
			name,
			layer,
			queue: Some(queue),
			thread: Some(thread),
			shared,
			skip_video: false,
		})
	}

	struct Worker {
		target: Target,
		shared: Arc<Shared>,
		packets: Receiver<Queued>,
		aac: AacEncoder,
		/// The tags being sent.
		buf: Vec<u8>,
	}

	/// What one connection has sent.
	#[derive(Default)]
	struct Publish {
		/// The sequence headers went out (at the first keyframe), and the
		/// stream's time 0 (milliseconds on the studio's clock).
		base: Option<i64>,
		/// AAC frames (milliseconds on the studio's clock, data) from before
		/// the first keyframe.
		backlog: VecDeque<(i64, Vec<u8>)>,
		/// AAC frames (stream time) waiting for the video of their time.
		audio: VecDeque<(i64, Vec<u8>)>,
		/// The stream time of the last video tag.
		video: Option<i64>,
		/// When the last keyframe request went out, while waiting for one.
		asked: Option<Instant>,
	}

	impl Worker {
		fn stopped(&self) -> bool {
			self.shared.stop.load(Ordering::Relaxed)
		}

		fn open(&self) -> std::result::Result<Connection, String> {
			let options: Vec<(&str, &str)> =
				self.target.options.iter().map(|(k, v)| (*k, v.as_str())).collect();
			Connection::open(&self.target.url, &options, self.shared.abort.clone())
		}

		fn run(mut self, ready: tokio::sync::oneshot::Sender<std::result::Result<(), String>>) {
			let mut connection = match self.open() {
				Ok(connection) => {
					let _ = ready.send(Ok(()));
					Some(connection)
				}
				Err(e) => {
					let _ = ready.send(Err(e));
					return;
				}
			};
			let mut backoff = BACKOFF.0;
			while !self.stopped() {
				let connection = match connection.take() {
					Some(connection) => connection,
					None => {
						if !self.wait(backoff) {
							break;
						}
						match self.open() {
							Ok(connection) => {
								self.shared.reconnects.fetch_add(1, Ordering::Relaxed);
								backoff = BACKOFF.0;
								connection
							}
							Err(e) => {
								self.fail(format!("cannot connect again: {e}"));
								backoff = (backoff * 2).min(BACKOFF.1);
								continue;
							}
						}
					}
				};
				self.shared.connected.store(true, Ordering::Relaxed);
				*lock(&self.shared.error) = None;
				let result = self.publish(connection);
				self.shared.connected.store(false, Ordering::Relaxed);
				match result {
					Ok(()) => break,
					Err(e) => self.fail(e),
				}
			}
			debug!(name = self.target.name, "RTMP output closed");
		}

		/// Wait `time` while dropping what comes in; `false` once stopped.
		fn wait(&self, time: Duration) -> bool {
			let until = Instant::now() + time;
			while !self.stopped() {
				let now = Instant::now();
				if now >= until {
					return true;
				}
				match self.packets.recv_timeout((until - now).min(POLL)) {
					Ok(_) => {
						self.shared.dropped.fetch_add(1, Ordering::Relaxed);
					}
					Err(RecvTimeoutError::Timeout) => {}
					Err(RecvTimeoutError::Disconnected) => return false,
				}
			}
			false
		}

		fn fail(&self, message: String) {
			warn!(name = self.target.name, "RTMP: {message}");
			*lock(&self.shared.error) = Some(message);
		}

		/// Send what comes in over `connection` until stopped (`Ok`: closed
		/// cleanly) or until it fails.
		fn publish(&mut self, mut connection: Connection) -> std::result::Result<(), String> {
			connection.write(&flv::HEADER)?;
			let mut publish = Publish::default();
			self.ask_keyframe(&mut publish);
			loop {
				if self.stopped() {
					return connection.close();
				}
				match self.packets.recv_timeout(POLL) {
					Ok(packet) => self.packet(&mut connection, &mut publish, packet)?,
					Err(RecvTimeoutError::Timeout) => {}
					Err(RecvTimeoutError::Disconnected) => return connection.close(),
				}
			}
		}

		fn ask_keyframe(&self, publish: &mut Publish) {
			publish.asked = Some(Instant::now());
			self.shared.keyframe.store(true, Ordering::Relaxed);
		}

		fn packet(
			&mut self,
			connection: &mut Connection,
			publish: &mut Publish,
			packet: Queued,
		) -> std::result::Result<(), String> {
			self.buf.clear();
			if packet.video {
				self.video(publish, &packet);
			} else {
				self.audio(publish, &packet);
			}
			if self.buf.is_empty() {
				return Ok(());
			}
			connection.write(&self.buf)?;
			connection.flush()?;
			self.shared.bytes.fetch_add(self.buf.len() as u64, Ordering::Relaxed);
			Ok(())
		}

		/// Append the tags a video packet makes to `self.buf`.
		fn video(&mut self, publish: &mut Publish, packet: &Queued) {
			let ms = (packet.pts_90khz / 90) as i64;
			let base = match publish.base {
				Some(base) => base,
				None => {
					// The stream starts at a keyframe that carries the
					// parameter sets the AVC sequence header needs.
					let avcc = packet.keyframe.then(|| ebml::avcc(&packet.data)).flatten();
					let Some(avcc) = avcc else {
						if publish.asked.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
							self.ask_keyframe(publish);
						}
						return;
					};
					let meta = flv::Metadata {
						width: packet.width,
						height: packet.height,
						sample_rate: RATE,
						channels: 2,
					};
					self.tag(flv::SCRIPT, 0, |b| flv::metadata(b, &meta));
					self.tag(flv::VIDEO, 0, |b| flv::avc_sequence_header(b, &avcc));
					self.tag(flv::AUDIO, 0, |b| {
						flv::aac_sequence_header(b, &AUDIO_SPECIFIC_CONFIG)
					});
					publish.base = Some(ms);
					// The audio of the keyframe's moment came before it.
					for (time, data) in publish.backlog.drain(..) {
						if time >= ms {
							publish.audio.push_back((time - ms, data));
						}
					}
					ms
				}
			};
			let time = (ms - base).max(publish.video.unwrap_or(0));
			while publish.audio.front().is_some_and(|(t, _)| *t <= time) {
				let (t, data) = publish.audio.pop_front().expect("checked");
				self.tag(flv::AUDIO, t, |b| flv::aac_frame(b, &data));
			}
			self.tag(flv::VIDEO, time, |b| flv::avc_frame(b, packet.keyframe, &packet.data));
			publish.video = Some(time);
		}

		/// Encode an Opus packet as AAC and append the audio tags that are
		/// due to `self.buf`.
		fn audio(&mut self, publish: &mut Publish, packet: &Queued) {
			let time = (packet.pts_90khz * u64::from(RATE) / 90_000) as i64;
			let mut frames = Vec::new();
			let result = self.aac.push(&packet.data, time, &mut |data, at| {
				frames.push((at * 1000 / i64::from(RATE), data.to_vec()));
			});
			if let Err(e) = result {
				debug!("RTMP audio: {e}");
			}
			for (ms, data) in frames {
				match publish.base {
					None => {
						publish.backlog.push_back((ms, data));
						while publish.backlog.front().is_some_and(|(t, _)| *t + BACKLOG_MS < ms) {
							publish.backlog.pop_front();
						}
					}
					Some(base) if ms >= base => publish.audio.push_back((ms - base, data)),
					Some(_) => {}
				}
			}
			// Audio goes out with the video of its time, or once the video
			// is late by more than INTERLEAVE_MS.
			let newest = publish.audio.back().map_or(0, |(t, _)| *t);
			let video = publish.video.unwrap_or(-1);
			while publish
				.audio
				.front()
				.is_some_and(|(t, _)| *t <= video || *t + INTERLEAVE_MS < newest)
			{
				let (t, data) = publish.audio.pop_front().expect("checked");
				self.tag(flv::AUDIO, t, |b| flv::aac_frame(b, &data));
			}
		}

		fn tag(&mut self, kind: u8, ms: i64, body: impl FnOnce(&mut Vec<u8>)) {
			flv::tag(&mut self.buf, kind, ms.clamp(0, i64::from(u32::MAX)) as u32, body);
			self.shared.tags.fetch_add(1, Ordering::Relaxed);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn urls_and_keys() {
		assert!(Rtmp::handles("rtmp://live.example/app"));
		assert!(Rtmp::handles("RTMPS://live.example/app"));
		assert!(!Rtmp::handles("https://example/whip"));

		// The key in the URL: FFmpeg splits it; the name hides it.
		let t = Target::new("rtmp://live.example/app/secret-key", None).unwrap();
		assert_eq!(t.url, "rtmp://live.example/app/secret-key");
		assert_eq!(t.name, "rtmp://live.example/app/…");
		assert!(!t.options.iter().any(|(k, _)| k.starts_with("rtmp_")));

		// The key apart: the play path, the URL's path the application.
		let t = Target::new("rtmps://a.rtmp.example:443/live2/", Some(" key?x=1 ")).unwrap();
		assert_eq!(t.name, "rtmps://a.rtmp.example:443/live2");
		assert!(t.options.contains(&("rtmp_app", "live2".to_owned())));
		assert!(t.options.contains(&("rtmp_playpath", "key?x=1".to_owned())));
		assert!(t.options.contains(&("rw_timeout", "10000000".to_owned())));
		assert!(!t.name.contains("key"));

		assert!(Target::new("http://example/app", None).is_err());
		assert!(Target::new("rtmp:///app", None).is_err());
	}

	#[tokio::test]
	async fn other_codecs_and_dead_servers_are_refused() {
		let e = Rtmp::start("rtmp://127.0.0.1:1/app", Some("k"), 0, Some(Codec::Vp8))
			.await
			.err()
			.expect("VP8 refused")
			.to_string();
		assert!(e.contains("H.264"), "{e}");
		// Port 1 on loopback: nothing listens.
		if crate::ffmpeg::avio::Connection::available().is_ok() {
			let e = Rtmp::start("rtmp://127.0.0.1:1/app", Some("k"), 0, Some(Codec::H264))
				.await
				.err()
				.expect("nothing listens")
				.to_string();
			assert!(!e.is_empty());
		}
	}
}
