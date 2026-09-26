//! Audio thread: capture → processing (echo cancellation, noise suppression,
//! gain) → voice activity / push-to-talk → encode → voice connection, and
//! incoming voice → jitter buffer/mixer (per-client volume) → playback, with
//! what is played fed back to the echo canceller as its reference.
//!
//! Devices live on their own OS thread (cpal streams are not `Send` on every
//! platform); the connection task talks to it through channels. The device
//! independent part is [`Pipeline`]; the thread loop only moves samples
//! between it and the devices, reopening devices that go away.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, warn};
use tsc_audio::pcm::{self, FRAME_SAMPLES};
use tsc_audio::settings::TransmitMode;
use tsc_audio::{AudioSettings, Framer, Mixer, Processor, Vad, VoiceCodec, VoiceEncoder};
use tsclientlib::ClientId;
use tsproto_packets::packets::{AudioData, InAudioBuf, OutPacket};

pub(crate) enum AudioIn {
	Packet(InAudioBuf),
	Transmit(bool),
	InputMuted(bool),
	OutputMuted(bool),
	/// Devices, processing, voice activation and transmit mode.
	#[allow(dead_code, reason = "sent once the session exposes audio settings")]
	Settings(Box<AudioSettings>),
	/// Playback volume of one client, linear (1 = unchanged, up to 4).
	#[allow(dead_code, reason = "sent once the session exposes client volumes")]
	ClientVolume {
		client: ClientId,
		volume: f32,
	},
	#[allow(dead_code, reason = "sent once the session exposes client volumes")]
	ClientMuted {
		client: ClientId,
		muted: bool,
	},
	/// The client left; client ids are reused, so forget its volume.
	#[allow(dead_code, reason = "sent once the session exposes client volumes")]
	ClientLeft(ClientId),
}

pub(crate) struct AudioHandle {
	tx: std_mpsc::Sender<AudioIn>,
}

impl AudioHandle {
	pub fn send(&self, msg: AudioIn) {
		let _ = self.tx.send(msg);
	}
}

/// With voice activation, also send this many 20 ms frames from before the
/// detector opened, so word onsets are not cut.
const PRE_ROLL_FRAMES: usize = 2;
/// Capacity of the speaker queue; [`AudioSettings::playback_buffer_ms`] is
/// clamped below it.
#[cfg(feature = "audio-device")]
const PLAYBACK_CAPACITY_MS: u32 = 200;
/// Latency of the devices beyond our queues, as a hint for the echo
/// canceller (it estimates the actual delay itself).
#[cfg(feature = "audio-device")]
const DEVICE_LATENCY_MS: u32 = 20;
/// How long the loop sleeps between rounds.
const TICK: Duration = Duration::from_millis(5);

/// Start the audio thread. Encoded packets go to `outgoing`; problems (e.g.
/// no device) are reported through `errors`.
pub(crate) fn spawn(
	outgoing: mpsc::UnboundedSender<OutPacket>,
	errors: mpsc::UnboundedSender<String>,
	voice_activation: bool,
) -> AudioHandle {
	let transmit =
		if voice_activation { TransmitMode::VoiceActivation } else { TransmitMode::PushToTalk };
	let settings = AudioSettings { transmit, ..Default::default() };
	let (tx, rx) = std_mpsc::channel();
	thread::Builder::new()
		.name("tsc-audio".into())
		.spawn(move || run(rx, outgoing, errors, settings))
		.expect("spawn audio thread");
	AudioHandle { tx }
}

/// Everything between the devices and the connection.
struct Pipeline {
	settings: AudioSettings,
	encoder: VoiceEncoder,
	mixer: Mixer,
	processor: Processor,
	vad: Vad,
	framer: Framer,
	processed: Vec<f32>,
	pre_roll: VecDeque<Vec<f32>>,
	ptt: bool,
	input_muted: bool,
	output_muted: bool,
	was_sending: bool,
	/// Processed microphone audio, for tests.
	#[cfg(test)]
	tap: Vec<f32>,
}

impl Pipeline {
	fn new(settings: AudioSettings) -> tsc_audio::Result<Self> {
		let settings = sanitize(settings);
		Ok(Self {
			encoder: VoiceEncoder::new(VoiceCodec::Voice)?,
			mixer: Mixer::new(),
			processor: Processor::new(&settings.processing),
			vad: Vad::new(&settings.vad),
			framer: Framer::new(FRAME_SAMPLES),
			processed: Vec::new(),
			pre_roll: VecDeque::new(),
			ptt: false,
			input_muted: false,
			output_muted: false,
			was_sending: false,
			settings,
			#[cfg(test)]
			tap: Vec::new(),
		})
	}

	fn handle(&mut self, msg: AudioIn) {
		match msg {
			AudioIn::Packet(packet) => {
				let from = match packet.data().data() {
					AudioData::S2C { from, .. } | AudioData::S2CWhisper { from, .. } => *from,
					_ => return,
				};
				if let Err(error) = self.mixer.handle_packet(ClientId(from), packet) {
					debug!(%error, "dropped voice packet");
				}
			}
			AudioIn::Transmit(on) => self.ptt = on,
			AudioIn::InputMuted(m) => self.input_muted = m,
			AudioIn::OutputMuted(m) => self.output_muted = m,
			AudioIn::Settings(settings) => {
				let settings = sanitize(*settings);
				self.processor.apply(&settings.processing);
				self.vad.apply(&settings.vad);
				if settings.transmit != TransmitMode::VoiceActivation {
					self.vad.reset();
					self.pre_roll.clear();
				}
				self.settings = settings;
			}
			AudioIn::ClientVolume { client, volume } => self.mixer.set_volume(client, volume),
			AudioIn::ClientMuted { client, muted } => self.mixer.set_muted(client, muted),
			AudioIn::ClientLeft(client) => self.mixer.forget(client),
		}
	}

	/// Microphone audio in (mono, 48 kHz, any length); voice packets out.
	/// `delay_ms` is the playback-to-capture delay hint for the echo canceller.
	fn capture(&mut self, samples: &[f32], delay_ms: u32, send: &mut impl FnMut(OutPacket)) {
		self.processor.set_delay_ms(delay_ms);
		self.processed.clear();
		self.processor.capture(samples, &mut self.processed);
		#[cfg(test)]
		self.tap.extend_from_slice(&self.processed);
		let mut frames = Vec::new();
		self.framer.push(&self.processed, |f| frames.push(f.to_vec()));
		for frame in frames {
			self.transmit(frame, send);
		}
	}

	fn transmit(&mut self, frame: Vec<f32>, send: &mut impl FnMut(OutPacket)) {
		// Always analysed, so the detector's state is current when needed.
		let voice = self.vad.process(&frame);
		let wanted = match self.settings.transmit {
			TransmitMode::PushToTalk => self.ptt,
			TransmitMode::VoiceActivation => voice,
			TransmitMode::Continuous => true,
		};
		let sending = wanted && !self.input_muted;
		if sending {
			let pre_roll: Vec<_> = self.pre_roll.drain(..).collect();
			for f in pre_roll.iter().chain([&frame]) {
				match self.encoder.encode(f) {
					Ok(packet) => send(packet),
					Err(e) => warn!(%e, "encoding failed"),
				}
			}
		} else {
			if self.was_sending {
				send(self.encoder.end_of_stream());
			}
			if self.settings.transmit == TransmitMode::VoiceActivation {
				self.pre_roll.push_back(frame);
				if self.pre_roll.len() > PRE_ROLL_FRAMES {
					self.pre_roll.pop_front();
				}
			}
		}
		self.was_sending = sending;
	}

	/// The next 20 ms for the speakers (mono, 48 kHz), which also becomes
	/// the echo canceller's reference.
	fn playback_frame(&mut self) -> Vec<f32> {
		let mut stereo = vec![0.0; FRAME_SAMPLES * 2];
		self.mixer.fill_buffer(&mut stereo);
		let gain = if self.output_muted { 0.0 } else { self.settings.output_volume };
		let mut mono = pcm::to_mono(&stereo, 2);
		for s in &mut mono {
			*s = (*s * gain).clamp(-1.0, 1.0);
		}
		self.processor.render(&mono);
		mono
	}
}

fn sanitize(mut settings: AudioSettings) -> AudioSettings {
	let volume = settings.output_volume;
	settings.output_volume = if volume.is_finite() { volume.clamp(0.0, 4.0) } else { 1.0 };
	#[cfg(feature = "audio-device")]
	{
		settings.playback_buffer_ms =
			settings.playback_buffer_ms.clamp(20, PLAYBACK_CAPACITY_MS - 20);
	}
	settings
}

fn run(
	rx: std_mpsc::Receiver<AudioIn>,
	outgoing: mpsc::UnboundedSender<OutPacket>,
	errors: mpsc::UnboundedSender<String>,
	settings: AudioSettings,
) {
	#[cfg(feature = "audio-device")]
	let mut devices = devices::Devices::new(&settings);
	#[cfg(not(feature = "audio-device"))]
	let _ = errors.send("built without audio device support".to_string());

	let mut pipeline = match Pipeline::new(settings) {
		Ok(p) => p,
		Err(e) => {
			let _ = errors.send(e.to_string());
			return;
		}
	};
	let mut send = |packet| {
		let _ = outgoing.send(packet);
	};
	// Without speakers the mixer is drained at real time, so queues do not
	// fill up and talkers still end.
	let mut idle_clock = Instant::now();

	loop {
		// Control messages and incoming voice.
		loop {
			match rx.try_recv() {
				Ok(msg) => {
					#[cfg(feature = "audio-device")]
					if let AudioIn::Settings(s) = &msg {
						devices.select(s);
					}
					pipeline.handle(msg);
				}
				Err(std_mpsc::TryRecvError::Empty) => break,
				Err(std_mpsc::TryRecvError::Disconnected) => return,
			}
		}

		#[cfg(feature = "audio-device")]
		let playing = devices.step(&mut pipeline, &mut send, &errors);
		#[cfg(not(feature = "audio-device"))]
		let playing = {
			// No microphone: nothing to process, but keep the path alive.
			pipeline.capture(&[], 0, &mut send);
			false
		};

		if playing {
			idle_clock = Instant::now();
		} else {
			let frame = Duration::from_millis(20);
			while idle_clock + frame <= Instant::now() {
				pipeline.playback_frame();
				idle_clock += frame;
			}
		}

		thread::sleep(TICK);
	}
}

#[cfg(feature = "audio-device")]
mod devices {
	use tokio::sync::mpsc;
	use tracing::debug;
	use tsc_audio::AudioSettings;
	use tsc_audio::device::{Capture, DeviceEvent, Managed, Playback};
	use tsc_audio::pcm::{self, FRAME_SAMPLES, SAMPLE_RATE};
	use tsc_audio::resample::Linear;
	use tsproto_packets::packets::OutPacket;

	use super::{DEVICE_LATENCY_MS, PLAYBACK_CAPACITY_MS, Pipeline};

	/// The microphone and speakers, reopened when they go away.
	pub(super) struct Devices {
		capture: Managed<Capture>,
		playback: Managed<Playback>,
		capture_resampler: Option<(u32, Linear)>,
		playback_resampler: Option<(u32, Linear)>,
		captured: Vec<f32>,
		resampled: Vec<f32>,
	}

	/// A resampler for this device rate, recreated when the rate changes
	/// (another device was opened).
	fn resampler(slot: &mut Option<(u32, Linear)>, from: u32, to: u32, rate: u32) -> &mut Linear {
		if slot.as_ref().is_none_or(|(r, _)| *r != rate) {
			*slot = Some((rate, Linear::new(from, to)));
		}
		&mut slot.as_mut().expect("just set").1
	}

	fn report(errors: &mpsc::UnboundedSender<String>, what: &str, event: DeviceEvent) {
		let message = match event {
			DeviceEvent::Opened { name, fallback: false } => {
				debug!(%name, "{what} opened");
				return;
			}
			DeviceEvent::Opened { name, fallback: true } => {
				format!("selected {what} not found, using {name}")
			}
			DeviceEvent::Lost { name } => format!("{what} disconnected: {name}"),
			DeviceEvent::Unavailable(e) => format!("no {what}: {e}"),
		};
		let _ = errors.send(message);
	}

	impl Devices {
		pub fn new(settings: &AudioSettings) -> Self {
			Self {
				capture: Managed::new(settings.input_device.clone(), 500),
				playback: Managed::new(settings.output_device.clone(), PLAYBACK_CAPACITY_MS),
				capture_resampler: None,
				playback_resampler: None,
				captured: Vec::new(),
				resampled: Vec::new(),
			}
		}

		pub fn select(&mut self, settings: &AudioSettings) {
			self.capture.select(settings.input_device.clone());
			self.playback.select(settings.output_device.clone());
		}

		/// One round: top up the speakers, then process what the microphone
		/// recorded. Returns whether speakers are open.
		pub fn step(
			&mut self,
			pipeline: &mut Pipeline,
			send: &mut impl FnMut(OutPacket),
			errors: &mpsc::UnboundedSender<String>,
		) -> bool {
			if let Some(event) = self.playback.poll() {
				report(errors, "speakers", event);
			}
			if let Some(event) = self.capture.poll() {
				report(errors, "microphone", event);
			}

			// Mixer → speakers, keeping the configured amount queued.
			let mut delay_ms = DEVICE_LATENCY_MS;
			let playing = if let Some(p) = self.playback.stream() {
				let r = resampler(&mut self.playback_resampler, SAMPLE_RATE, p.rate, p.rate);
				let frame_len =
					(FRAME_SAMPLES * p.rate as usize).div_ceil(SAMPLE_RATE as usize) + 1;
				while p.queued_ms() < pipeline.settings.playback_buffer_ms
					&& p.free() >= frame_len * p.channels
				{
					let mono = pipeline.playback_frame();
					let mut out = Vec::with_capacity(frame_len);
					r.process(&mono, &mut out);
					p.write(&pcm::from_mono(&out, p.channels));
				}
				delay_ms += p.queued_ms();
				true
			} else {
				false
			};

			// Microphone → 48 kHz mono → pipeline.
			if let Some(c) = self.capture.stream() {
				self.captured.clear();
				self.resampled.clear();
				c.read_available(&mut self.captured);
				let mono = pcm::to_mono(&self.captured, c.channels);
				let r = resampler(&mut self.capture_resampler, c.rate, SAMPLE_RATE, c.rate);
				r.process(&mono, &mut self.resampled);
				pipeline.capture(&self.resampled, delay_ms, send);
			}
			playing
		}
	}
}

#[cfg(test)]
mod tests {
	use tsc_audio::pcm::{SAMPLE_RATE, energy, sine, white_noise};
	use tsc_audio::{ProcessingSettings, VadSettings};
	use tsproto_packets::packets::{CodecType, Direction, OutAudio};

	use super::*;

	fn pipeline(transmit: TransmitMode) -> Pipeline {
		Pipeline::new(AudioSettings {
			transmit,
			processing: ProcessingSettings::off(),
			vad: VadSettings { hangover_ms: 40, ..Default::default() },
			..Default::default()
		})
		.unwrap()
	}

	/// Run `samples` through the capture side; returns (voice packets,
	/// end-of-stream packets).
	fn capture(p: &mut Pipeline, samples: &[f32]) -> (usize, usize) {
		let (mut voice, mut end) = (0, 0);
		p.capture(samples, 0, &mut |packet: OutPacket| {
			// Header of 3 bytes (id + codec) for C2S audio.
			if packet.content().len() > 3 { voice += 1 } else { end += 1 }
		});
		(voice, end)
	}

	fn incoming(from: u16, id: u16, data: &[u8]) -> AudioIn {
		let packet = OutAudio::new(&AudioData::S2C { id, codec: CodecType::OpusVoice, from, data });
		AudioIn::Packet(InAudioBuf::try_new(Direction::S2C, packet.into_vec()).unwrap())
	}

	#[test]
	fn push_to_talk_and_mute() {
		let mut p = pipeline(TransmitMode::PushToTalk);
		let tone = sine(440.0, 0.2, 0.3);
		assert_eq!(capture(&mut p, &tone), (0, 0));
		p.handle(AudioIn::Transmit(true));
		assert_eq!(capture(&mut p, &tone), (10, 0));
		p.handle(AudioIn::InputMuted(true));
		assert_eq!(capture(&mut p, &tone), (0, 1));
		p.handle(AudioIn::InputMuted(false));
		p.handle(AudioIn::Transmit(false));
		assert_eq!(capture(&mut p, &tone), (0, 0));
	}

	#[test]
	fn voice_activation_with_pre_roll() {
		let mut p = pipeline(TransmitMode::VoiceActivation);
		let quiet = vec![0.0; FRAME_SAMPLES * 5];
		assert_eq!(capture(&mut p, &quiet), (0, 0));
		// Speech opens the gate: the frame itself plus the pre-roll.
		assert_eq!(capture(&mut p, &sine(440.0, 0.02, 0.3)), (1 + PRE_ROLL_FRAMES, 0));
		// 40 ms of hangover, then the stream ends.
		assert_eq!(capture(&mut p, &quiet), (2, 1));

		// Switching modes through settings.
		let settings = AudioSettings { transmit: TransmitMode::Continuous, ..p.settings.clone() };
		p.handle(AudioIn::Settings(Box::new(settings)));
		assert_eq!(capture(&mut p, &quiet), (5, 0));
	}

	/// Two talkers; one turned down, one muted, then forgotten.
	#[test]
	fn client_volume() {
		let mut p = pipeline(TransmitMode::PushToTalk);
		let mut encoder = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let tone = sine(700.0, 1.0, 0.4);
		let mut play = |p: &mut Pipeline, from: u16| -> f32 {
			let mut out = Vec::new();
			for (i, frame) in tone.chunks_exact(FRAME_SAMPLES).enumerate() {
				p.handle(incoming(from, i as u16, encoder.encode_to_bytes(frame).unwrap()));
				out.extend(p.playback_frame());
			}
			energy(&out[out.len() / 2..])
		};
		let full = play(&mut p, 1);
		p.handle(AudioIn::ClientVolume { client: ClientId(2), volume: 0.5 });
		let half = play(&mut p, 2);
		let ratio_db = 10.0 * (half / full).log10();
		assert!((ratio_db + 6.0).abs() < 1.0, "volume 0.5 gave {ratio_db} dB");
		p.handle(AudioIn::ClientMuted { client: ClientId(3), muted: true });
		assert!(play(&mut p, 3) < 1e-8);
		p.handle(AudioIn::ClientLeft(ClientId(3)));
		assert!(!p.mixer.is_muted(ClientId(3)));
	}

	/// End to end: a remote talker is played, the speakers leak into the
	/// microphone 30 ms later, and the echo canceller (fed with what was
	/// played) removes it from what we would send.
	#[test]
	fn echo_of_playback_is_cancelled() {
		let mut p = Pipeline::new(AudioSettings {
			transmit: TransmitMode::Continuous,
			processing: ProcessingSettings { echo_cancellation: true, ..ProcessingSettings::off() },
			..Default::default()
		})
		.unwrap();
		let mut encoder = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let far = white_noise(SAMPLE_RATE as usize * 6, 0.3, 11);
		let delay = SAMPLE_RATE as usize * 30 / 1000;
		let mut played = vec![0.0; delay];
		let mut echo = Vec::new();
		for (i, frame) in far.chunks_exact(FRAME_SAMPLES).enumerate() {
			p.handle(incoming(9, i as u16, encoder.encode_to_bytes(frame).unwrap()));
			played.extend(p.playback_frame());
			let mic: Vec<f32> = played[played.len() - delay - FRAME_SAMPLES..played.len() - delay]
				.iter()
				.map(|s| s * 0.5)
				.collect();
			p.capture(&mic, 30, &mut |_| {});
			echo.extend(mic);
		}
		let tail = SAMPLE_RATE as usize * 2;
		let before = energy(&echo[echo.len() - tail..]);
		let after = energy(&p.tap[p.tap.len() - tail..]);
		let attenuation = 10.0 * (before / after).log10();
		assert!(before > 1e-3, "the echo must be audible");
		assert!(attenuation >= 10.0, "echo attenuated by {attenuation:.1} dB");
	}

	#[test]
	fn settings_are_sanitized() {
		let mut p = pipeline(TransmitMode::PushToTalk);
		let settings = AudioSettings { output_volume: f32::INFINITY, ..Default::default() };
		p.handle(AudioIn::Settings(Box::new(settings)));
		assert_eq!(p.settings.output_volume, 1.0);
	}
}
