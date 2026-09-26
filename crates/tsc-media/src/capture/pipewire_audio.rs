//! System audio on Linux: a PipeWire capture stream on the monitor of the
//! default sink (`stream.capture.sink = true`), converted by PipeWire to
//! 48 kHz stereo `f32`.
//!
//! The monitor carries everything the sink plays, including this app's own
//! playback (TeamSpeak voices). PipeWire has no "all but this process"
//! monitor; excluding our nodes would need a private null sink with links from
//! every other application's output node (tracked through the registry).
//! TODO; until then, the engine should play voices to a different sink or
//! accept that viewers hear them.

use std::time::Instant;

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::{Object, Value};
use pw::spa::utils::SpaTypes;
use tracing::warn;

use crate::capture::AudioCapture;
use crate::capture::pw::{PwThread, pod, serialize};
use crate::frame::{AUDIO_SAMPLE_RATE, AudioBuffer};
use crate::queue::{FrameReceiver, FrameSender, frame_channel};
use crate::{Error, Result};

const BACKEND: &str = "pipewire";

/// Name of our capture node in PipeWire.
pub const NODE_NAME: &str = "tsc-system-audio";

/// Captures the default sink's monitor through PipeWire.
#[derive(Default)]
pub struct PipeWireAudioCapture {
	thread: Option<PwThread>,
}

impl PipeWireAudioCapture {
	pub fn new() -> Self {
		Self::default()
	}
}

impl AudioCapture for PipeWireAudioCapture {
	fn backend(&self) -> &'static str {
		BACKEND
	}

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>> {
		self.stop();
		// Half a second at PipeWire's usual 10-20 ms quantum.
		let (tx, rx) = frame_channel(50);
		let thread = PwThread::spawn("tsc-pipewire-audio", None, move |core, mainloop| {
			audio_stream(core, mainloop, tx)
		})
		.map_err(|reason| Error::CaptureUnavailable { backend: BACKEND, reason })?;
		self.thread = Some(thread);
		Ok(rx)
	}

	fn stop(&mut self) {
		self.thread = None;
	}
}

struct AudioState {
	tx: FrameSender<AudioBuffer>,
	channels: u16,
	started: Instant,
	mainloop: pw::main_loop::MainLoopWeak,
}

fn audio_stream(
	core: &pw::core::CoreRc,
	mainloop: &pw::main_loop::MainLoopRc,
	tx: FrameSender<AudioBuffer>,
) -> std::result::Result<(pw::stream::StreamRc, pw::stream::StreamListener<AudioState>), String> {
	let props = pw::properties::properties! {
		*pw::keys::MEDIA_TYPE => "Audio",
		*pw::keys::MEDIA_CATEGORY => "Capture",
		*pw::keys::MEDIA_ROLE => "Music",
		*pw::keys::STREAM_CAPTURE_SINK => "true",
		*pw::keys::NODE_NAME => NODE_NAME,
		*pw::keys::NODE_DESCRIPTION => "TeamSpeak stream system audio",
	};
	let stream = pw::stream::StreamRc::new(core.clone(), NODE_NAME, props)
		.map_err(|e| format!("PipeWire stream: {e}"))?;
	let state =
		AudioState { tx, channels: 2, started: Instant::now(), mainloop: mainloop.downgrade() };
	let listener = stream
		.add_local_listener_with_user_data(state)
		.state_changed(|_, state, _, new| {
			if let pw::stream::StreamState::Error(e) = &new {
				warn!("system audio stream failed: {e}");
				if let Some(mainloop) = state.mainloop.upgrade() {
					mainloop.quit();
				}
			}
		})
		.param_changed(|_, state, id, param| {
			let Some(param) = param else { return };
			if id != pw::spa::param::ParamType::Format.as_raw() {
				return;
			}
			let mut info = AudioInfoRaw::new();
			if info.parse(param).is_ok() && info.channels() > 0 {
				state.channels = info.channels() as u16;
			}
		})
		.process(|stream, state| {
			let Some(mut buffer) = stream.dequeue_buffer() else { return };
			let Some(data) = buffer.datas_mut().first_mut() else { return };
			let chunk = data.chunk();
			let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
			let Some(bytes) = data.data() else { return };
			let Some(bytes) = bytes.get(offset..offset + size) else { return };
			let samples: Vec<f32> = bytes
				.chunks_exact(4)
				.map(|b| f32::from_le_bytes(b.try_into().expect("4 bytes")))
				.collect();
			if samples.is_empty() {
				return;
			}
			let buffer = AudioBuffer {
				samples,
				channels: state.channels,
				timestamp: state.started.elapsed(),
			};
			if !state.tx.send(buffer)
				&& let Some(mainloop) = state.mainloop.upgrade()
			{
				mainloop.quit();
			}
		})
		.register()
		.map_err(|e| format!("PipeWire listener: {e}"))?;

	let mut info = AudioInfoRaw::new();
	info.set_format(AudioFormat::F32LE);
	info.set_rate(AUDIO_SAMPLE_RATE);
	info.set_channels(2);
	let format = serialize(Value::Object(Object {
		type_: SpaTypes::ObjectParamFormat.as_raw(),
		id: pw::spa::param::ParamType::EnumFormat.as_raw(),
		properties: info.into(),
	}))?;
	stream
		.connect(
			pw::spa::utils::Direction::Input,
			None,
			pw::stream::StreamFlags::AUTOCONNECT
				| pw::stream::StreamFlags::MAP_BUFFERS
				| pw::stream::StreamFlags::RT_PROCESS,
			&mut [pod(&format)?],
		)
		.map_err(|e| format!("cannot connect the system audio stream: {e}"))?;
	Ok((stream, listener))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Without a PipeWire daemon, start fails with a clear error instead of
	/// hanging. Skipped where a daemon socket exists.
	#[test]
	fn unavailable_without_daemon() {
		let socket = std::env::var_os("XDG_RUNTIME_DIR")
			.is_some_and(|d| std::path::Path::new(&d).join("pipewire-0").exists());
		if socket || std::env::var_os("PIPEWIRE_REMOTE").is_some() {
			eprintln!("skipped: a PipeWire daemon is running");
			return;
		}
		let mut capture = PipeWireAudioCapture::new();
		let err = capture.start().err().expect("no daemon");
		assert!(matches!(err, Error::CaptureUnavailable { backend: "pipewire", .. }), "{err}");
		assert!(err.to_string().contains("PipeWire"), "{err}");
	}
}
