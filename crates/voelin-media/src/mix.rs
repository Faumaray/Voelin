//! The stream's audio mixer: any number of sources into one 48 kHz stereo
//! signal ([`StreamMixer`]).
//!
//! - A source ([`MixerHandle::add_source`]) has a name, a gain and a mute,
//!   and any number of inputs ([`SourceHandle::input`]): lock-free
//!   single-producer rings that a capture thread fills at its own pace, at
//!   any sample rate and channel count. Channels are mapped to stereo as
//!   they are pushed; the rate is converted while mixing.
//! - The mixer runs on the caller's clock, one block at a time
//!   ([`StreamMixer::mix`], paced by [`BlockClock`]). Each input is held at
//!   a small fixed latency ([`MixerConfig::latency`], more for an input that
//!   delivers in larger bursts or keeps arriving late): it starts once that
//!   much is buffered, and an input that runs dry (its capture stalled,
//!   nothing plays, or it ended) fades out and waits to buffer again while
//!   every other source goes on. Clock drift between a source and the mixer
//!   is absorbed by resampling up to 0.5 % faster or slower while the
//!   buffer is off target; the rest of the time a 48 kHz input is copied
//!   through bit-exact.
//! - Rate conversion is cubic (Catmull-Rom) interpolation per input.
//! - The sum goes through a master gain and a soft limiter (a peak envelope
//!   with 1 ms attack, gain reduction above the threshold, and a soft clip
//!   of what the envelope is too slow for), so the output never exceeds
//!   full scale; below the threshold it is untouched.
//! - Levels (peak falling back at 20 dB/s, RMS over about 300 ms) of each
//!   source (after its gain, before its mute) and of the output are atomics:
//!   [`SourceHandle::level`] and [`MixerHandle::level`] read them without
//!   locking, from any thread.
//! - Buffers are allocated when a source or an input is created, sized from
//!   the [`MixerConfig`]; mixing works in place in them and allocates
//!   nothing per block. Sources, inputs, gains and mutes change while
//!   mixing: control calls queue changes that the mixer picks up at its next
//!   block (with a non-blocking try-lock that only contends with those
//!   calls).

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::frame::AUDIO_SAMPLE_RATE;

/// Output rate of the mixer.
pub const MIX_RATE: u32 = AUDIO_SAMPLE_RATE;

/// One stereo frame: left, right.
pub type Frame = [f32; 2];

const SILENCE: Frame = [0.0; 2];
/// The most the drift correction speeds an input up or slows it down.
const MAX_CORRECTION: f64 = 0.005;
/// Buffer error (seconds) above which the drift correction starts ...
const DRIFT_ENGAGE: f64 = 0.010;
/// ... and below which it winds down again.
const DRIFT_RELEASE: f64 = 0.002;
/// The correction aims to remove the error within this many seconds.
const DRIFT_TIME: f64 = 4.0;
/// Time constant of an input's average buffer level (seconds).
const FILL_TIME: f64 = 1.0;
/// Time constant (seconds) in which an input's largest burst is forgotten.
const BURST_TIME: f64 = 15.0;
/// Headroom beyond the largest burst and a block (seconds).
const BURST_MARGIN: f64 = 0.005;
/// Frames faded in when an input starts, and out when it runs dry.
const FADE: usize = 32;
/// Latency added to an input that ran dry and came back within
/// [`HICCUP`]: it delivers too unevenly for its latency.
const UNDERRUN_STEP: f64 = 0.010;
const HICCUP: f64 = 0.25;
/// Peak meters fall back this fast (dB per second).
const PEAK_FALL: f32 = 20.0;
/// Time constant of the RMS meters (seconds).
const RMS_TIME: f32 = 0.3;
/// Attack of the limiter's envelope (seconds).
const LIMITER_ATTACK: f32 = 0.001;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How the mixer is set up; buffers are sized from it.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerConfig {
	/// Largest block [`StreamMixer::mix`] handles at once, in frames
	/// (longer calls are split). Default 960: 20 ms, one Opus frame.
	pub max_block: usize,
	/// How far behind its capture each input is mixed, to absorb uneven
	/// delivery. Inputs that deliver in larger bursts get more. Default
	/// 40 ms.
	pub latency: Duration,
	/// Ring size of each input: how far ahead of the mixer a capture can
	/// get before audio is dropped. Default 500 ms.
	pub buffer: Duration,
	/// Soft-limiter threshold, linear (0 to 1]. Default -1 dBFS.
	pub limiter_threshold: f32,
	/// How fast the limiter lets go. Default 100 ms.
	pub limiter_release: Duration,
}

impl Default for MixerConfig {
	fn default() -> Self {
		Self {
			max_block: 960,
			latency: Duration::from_millis(40),
			buffer: Duration::from_millis(500),
			limiter_threshold: 0.891,
			limiter_release: Duration::from_millis(100),
		}
	}
}

/// An `f32` in an `AtomicU32` (relaxed: every value is complete on its own).
struct AtomicF32(AtomicU32);

impl AtomicF32 {
	fn new(value: f32) -> Self {
		Self(AtomicU32::new(value.to_bits()))
	}

	fn load(&self) -> f32 {
		f32::from_bits(self.0.load(Ordering::Relaxed))
	}

	fn store(&self, value: f32) {
		self.0.store(value.to_bits(), Ordering::Relaxed);
	}
}

/// A meter reading, linear (1.0 is full scale).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level {
	/// Highest sample, falling back at 20 dB/s.
	pub peak: f32,
	/// Root mean square over about 300 ms (both channels).
	pub rms: f32,
}

impl Level {
	pub fn peak_db(&self) -> f32 {
		to_db(self.peak)
	}

	pub fn rms_db(&self) -> f32 {
		to_db(self.rms)
	}
}

/// dBFS of a linear level; -120 for silence.
pub fn to_db(level: f32) -> f32 {
	if level > 1e-6 { 20.0 * level.log10() } else { -120.0 }
}

/// A level for readers on other threads.
struct Meter {
	peak: AtomicF32,
	rms: AtomicF32,
}

impl Meter {
	fn new() -> Self {
		Self { peak: AtomicF32::new(0.0), rms: AtomicF32::new(0.0) }
	}

	fn load(&self) -> Level {
		Level { peak: self.peak.load(), rms: self.rms.load() }
	}
}

/// The mixer's side of a meter: fall-back and smoothing.
#[derive(Default)]
struct MeterState {
	peak: f32,
	mean_square: f32,
}

/// Fall-back and smoothing factors for one block.
#[derive(Clone, Copy)]
struct Ballistics {
	decay: f32,
	alpha: f32,
}

impl Ballistics {
	fn new(frames: usize) -> Self {
		let dt = frames as f32 / MIX_RATE as f32;
		Self { decay: 10f32.powf(-PEAK_FALL / 20.0 * dt), alpha: 1.0 - (-dt / RMS_TIME).exp() }
	}
}

impl MeterState {
	/// Take a block's peak and sum of squares (over `samples` samples).
	fn update(&mut self, peak: f32, squares: f32, samples: usize, b: Ballistics, meter: &Meter) {
		self.peak = peak.max(self.peak * b.decay);
		let mean = if samples == 0 { 0.0 } else { squares / samples as f32 };
		self.mean_square += b.alpha * (mean - self.mean_square);
		meter.peak.store(self.peak);
		meter.rms.store(self.mean_square.max(0.0).sqrt());
	}
}

/// What a source is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceState {
	/// Nothing buffered to play: no input yet, nothing plays, or the
	/// capture stalled. Silent until an input buffers its latency.
	#[default]
	Idle,
	/// At least one input plays.
	Playing,
	/// Every input ended (their captures stopped).
	Ended,
}

impl SourceState {
	fn from_u8(value: u8) -> Self {
		match value {
			1 => Self::Playing,
			2 => Self::Ended,
			_ => Self::Idle,
		}
	}
}

/// Counters of a source ([`SourceHandle::stats`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceStats {
	pub state: SourceState,
	/// Inputs feeding it.
	pub inputs: usize,
	/// How far behind their captures its playing inputs are mixed (the
	/// largest average buffer level).
	pub latency: Duration,
	/// Times an input ran dry while playing (and came back soon: delivery
	/// hiccups, not pauses).
	pub underruns: u64,
	/// Frames dropped because an input's ring was full (the mixer was held
	/// up or stopped).
	pub overflows: u64,
	/// Frames skipped to bring an input that got far ahead back to its
	/// latency.
	pub skipped: u64,
}

/// Burst size of an input, written by its producer.
struct LaneShared {
	max_push: AtomicUsize,
}

struct SourceShared {
	id: u64,
	name: String,
	gain: AtomicF32,
	muted: AtomicBool,
	removed: AtomicBool,
	meter: Meter,
	state: AtomicU8,
	inputs: AtomicUsize,
	latency_us: AtomicU64,
	underruns: AtomicU64,
	overflows: AtomicU64,
	skipped: AtomicU64,
	/// Inputs the mixer has not picked up yet.
	new_lanes: Mutex<Vec<Lane>>,
	has_new_lanes: AtomicBool,
}

struct MixerShared {
	config: MixerConfig,
	next_id: AtomicU64,
	/// The sources, in the order they were added (for listing).
	sources: Mutex<Vec<Arc<SourceShared>>>,
	/// Sources the mixer has not picked up yet.
	new_sources: Mutex<Vec<MixSource>>,
	has_new_sources: AtomicBool,
	gain: AtomicF32,
	meter: Meter,
	/// The limiter's current gain (1: not limiting).
	limiter: AtomicF32,
	/// Frames mixed.
	frames: AtomicU64,
}

/// One input as the mixer sees it: the consuming end of its ring, the
/// resampler window and the latency / drift state.
struct Lane {
	ring: rtrb::Consumer<Frame>,
	shared: Arc<LaneShared>,
	rate: u32,
	capacity: usize,
	/// Input frames per output frame, without drift correction.
	step: f64,
	/// Relative speed-up (positive) or slow-down of the drift correction.
	correction: f64,
	/// The correction is winding down (at 48 kHz: until the phase lines
	/// up with a sample again, so the input is copied bit-exact after).
	releasing: bool,
	/// Frames read from the ring and not passed yet: `window[0]` is the
	/// frame before the next output position, which is `pos` in [1, 2).
	window: Vec<Frame>,
	pos: f64,
	playing: bool,
	/// Played before: running dry counts as an underrun.
	started: bool,
	/// Frames faded in since starting.
	faded: usize,
	/// Average buffer level before a block is read (input frames).
	fill: f64,
	/// The level the drift correction keeps: where the input settled in
	/// its first second of playing.
	setpoint: Option<f64>,
	settle_sum: f64,
	settle_blocks: u32,
	/// Largest burst seen lately (input frames).
	burst: f64,
	/// Latency added after hiccups (input frames).
	extra: usize,
	/// Blocks since the input ran dry.
	dry_blocks: u64,
	ended: bool,
}

impl Lane {
	fn new(config: &MixerConfig, rate: u32) -> (Self, rtrb::Producer<Frame>, Arc<LaneShared>) {
		let rate = rate.max(1);
		let step = f64::from(rate) / f64::from(MIX_RATE);
		let window = max_input(config.max_block, step) + 8;
		let capacity =
			((config.buffer.as_secs_f64() * f64::from(rate)).ceil() as usize).max(4 * window);
		let (producer, ring) = rtrb::RingBuffer::new(capacity);
		let shared = Arc::new(LaneShared { max_push: AtomicUsize::new(0) });
		let lane = Self {
			ring,
			shared: shared.clone(),
			rate,
			capacity,
			step,
			correction: 0.0,
			releasing: false,
			window: Vec::with_capacity(window),
			pos: 1.0,
			playing: false,
			started: false,
			faded: 0,
			fill: 0.0,
			setpoint: None,
			settle_sum: 0.0,
			settle_blocks: 0,
			burst: 0.0,
			extra: 0,
			dry_blocks: 0,
			ended: false,
		};
		(lane, producer, shared)
	}

	/// Input frames needed for `n` output frames at most.
	fn max_input(&self, n: usize) -> usize {
		max_input(n, self.step)
	}

	/// Frames to buffer before playing.
	fn target(&self, n: usize, latency: f64) -> usize {
		let rate = f64::from(self.rate);
		let base = (latency * rate) as usize;
		let burst = self.burst as usize + self.max_input(n) + (BURST_MARGIN * rate) as usize;
		(base.max(burst) + self.extra).min(self.capacity * 3 / 4)
	}

	fn start(&mut self, avail: usize) {
		self.window.clear();
		self.window.push(SILENCE);
		self.pos = 1.0;
		self.correction = 0.0;
		self.releasing = false;
		self.faded = 0;
		self.playing = true;
		self.settle(avail);
		// Back soon after running dry: a hiccup, not a pause.
		let dry = self.dry_blocks as f64 * 0.02;
		if self.started && dry < HICCUP {
			self.extra += (UNDERRUN_STEP * f64::from(self.rate)) as usize;
		}
		self.started = true;
	}

	/// Render `out.len()` frames into `out` (overwriting it). Returns
	/// `false`, with `out` untouched, while the input is not playing.
	fn render(&mut self, out: &mut [Frame], latency: f64, source: &SourceShared) -> bool {
		let n = out.len();
		let recent = self.shared.max_push.swap(0, Ordering::Relaxed) as f64;
		let dt = n as f64 / f64::from(MIX_RATE);
		self.burst = recent.max(self.burst * (-dt / BURST_TIME).exp());
		let mut avail = self.ring.slots();
		let target = self.target(n, latency);
		if !self.playing {
			let abandoned = self.ring.is_abandoned();
			if avail >= target.max(1) || (abandoned && avail > 0) {
				self.start(avail);
			} else {
				self.ended = abandoned;
				self.dry_blocks += 1;
				return false;
			}
		}
		// Far ahead (the mixer was held up, or the source runs fast): skip
		// back to the target.
		if avail > 2 * target + self.max_input(n) {
			let skip = avail - target;
			if let Ok(chunk) = self.ring.read_chunk(skip) {
				chunk.commit_all();
			}
			source.skipped.fetch_add(skip as u64, Ordering::Relaxed);
			avail = target;
			self.settle(avail);
		}
		self.steer(n, avail);

		// Read what this block needs.
		let step = self.step * (1.0 + self.correction);
		let reach = if self.releasing { step.max(self.step) } else { step };
		let need = ((self.pos + (n - 1) as f64 * reach) as usize + 3).min(self.window.capacity());
		let take = need.saturating_sub(self.window.len()).min(avail);
		if take > 0
			&& let Ok(chunk) = self.ring.read_chunk(take)
		{
			let (a, b) = chunk.as_slices();
			self.window.extend_from_slice(a);
			self.window.extend_from_slice(b);
			chunk.commit_all();
		}

		let produced = self.interpolate(out);
		out[produced..].fill(SILENCE);
		// Fade in after starting.
		for frame in out[..produced].iter_mut() {
			if self.faded >= FADE {
				break;
			}
			self.faded += 1;
			let g = self.faded as f32 / (FADE + 1) as f32;
			*frame = [frame[0] * g, frame[1] * g];
		}
		if produced < n {
			// Ran dry: fade out what there was and wait for the latency again.
			let m = produced.min(FADE);
			for (j, frame) in out[produced - m..produced].iter_mut().enumerate() {
				let g = (m - j) as f32 / (m + 1) as f32;
				*frame = [frame[0] * g, frame[1] * g];
			}
			if !self.ring.is_abandoned() {
				source.underruns.fetch_add(1, Ordering::Relaxed);
			}
			self.playing = false;
			self.dry_blocks = 0;
		} else {
			// Keep one frame of history before the next position.
			let drop = (self.pos as usize).saturating_sub(1).min(self.window.len());
			self.window.drain(..drop);
			self.pos -= drop as f64;
		}
		true
	}

	/// Measure the level to keep afresh.
	fn settle(&mut self, avail: usize) {
		self.fill = avail as f64;
		self.setpoint = None;
		self.settle_sum = 0.0;
		self.settle_blocks = 0;
		self.correction = 0.0;
		self.releasing = false;
	}

	/// Drift correction: keep the average buffer level at the setpoint.
	/// Starting at a burst's peak or trough is no drift, so the setpoint is
	/// the average of the first second rather than the start threshold.
	fn steer(&mut self, n: usize, avail: usize) {
		let Some(setpoint) = self.setpoint else {
			self.settle_sum += avail as f64;
			self.settle_blocks += 1;
			self.fill = self.settle_sum / f64::from(self.settle_blocks);
			if f64::from(self.settle_blocks) * n as f64 >= FILL_TIME * f64::from(MIX_RATE) {
				self.setpoint = Some(self.fill);
			}
			return;
		};
		let alpha = 1.0 - (-(n as f64) / (f64::from(MIX_RATE) * FILL_TIME)).exp();
		self.fill += alpha * (avail as f64 - self.fill);
		let error = (self.fill - setpoint) / f64::from(self.rate);
		let wanted = (error / DRIFT_TIME).clamp(-MAX_CORRECTION, MAX_CORRECTION);
		if self.correction == 0.0 {
			if error.abs() > DRIFT_ENGAGE {
				self.correction = wanted;
				self.releasing = false;
			}
		} else if error.abs() < DRIFT_RELEASE || error.signum() != self.correction.signum() {
			self.releasing = true;
		} else if !self.releasing {
			self.correction = wanted;
		}
	}

	/// Interpolate from the window into `out`; returns the frames produced
	/// (fewer when the window runs out) and advances `pos`.
	fn interpolate(&mut self, out: &mut [Frame]) -> usize {
		let base = self.step;
		if self.releasing && base != 1.0 {
			// Changing the step keeps the phase: nothing to line up.
			self.correction = 0.0;
			self.releasing = false;
		}
		let len = self.window.len();
		let mut step = base * (1.0 + self.correction);
		let mut x = self.pos;
		if step == 1.0 && x.fract() == 0.0 {
			// 48 kHz in step: a plain copy.
			let i = x as usize;
			let count = out.len().min(len.saturating_sub(i + 2));
			out[..count].copy_from_slice(&self.window[i..i + count]);
			self.pos = x + count as f64;
			return count;
		}
		let snap = (step - base).abs();
		let w = &self.window;
		for (k, frame) in out.iter_mut().enumerate() {
			let i = x as usize;
			if i + 2 >= len {
				self.pos = x;
				return k;
			}
			let mut t = x - i as f64;
			if self.releasing && t < snap {
				// In phase with the input again: stop correcting.
				x = i as f64;
				t = 0.0;
				step = base;
				self.correction = 0.0;
				self.releasing = false;
			}
			let t = t as f32;
			let (p0, p1, p2, p3) = (w[i - 1], w[i], w[i + 1], w[i + 2]);
			*frame = [cubic(p0[0], p1[0], p2[0], p3[0], t), cubic(p0[1], p1[1], p2[1], p3[1], t)];
			x += step;
		}
		self.pos = x;
		out.len()
	}
}

/// Input frames for `n` output frames at `step`, with drift correction and
/// the interpolator's reach.
fn max_input(n: usize, step: f64) -> usize {
	(n as f64 * step * (1.0 + MAX_CORRECTION)).ceil() as usize + 4
}

/// Catmull-Rom interpolation between `p1` (t = 0, returned exactly) and
/// `p2` (t = 1).
#[inline]
fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
	let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
	let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
	let c = 0.5 * (p2 - p0);
	((a * t + b) * t + c) * t + p1
}

/// A source as the mixer sees it.
struct MixSource {
	shared: Arc<SourceShared>,
	lanes: Vec<Lane>,
	/// Gain and mute (1 or 0) the last block ended with; the next block
	/// ramps from these to the current settings.
	gain: f32,
	mute: f32,
	had_lanes: bool,
	meter: MeterState,
}

/// Envelope-following soft limiter.
struct Limiter {
	threshold: f32,
	attack: f32,
	release: f32,
	envelope: f32,
	/// Lowest gain of the last block, for [`MixerHandle::limiter_gain`].
	lowest: f32,
}

impl Limiter {
	fn new(config: &MixerConfig) -> Self {
		let coeff = |seconds: f32| (-1.0 / (seconds.max(1e-4) * MIX_RATE as f32)).exp();
		Self {
			threshold: config.limiter_threshold.clamp(0.01, 1.0),
			attack: coeff(LIMITER_ATTACK),
			release: coeff(config.limiter_release.as_secs_f32()),
			envelope: 0.0,
			lowest: 1.0,
		}
	}

	#[inline]
	fn process(&mut self, frame: &mut Frame) {
		let peak = frame[0].abs().max(frame[1].abs());
		let coeff = if peak > self.envelope { self.attack } else { self.release };
		self.envelope = peak + coeff * (self.envelope - peak);
		if self.envelope > self.threshold {
			let gain = self.threshold / self.envelope;
			self.lowest = self.lowest.min(gain);
			frame[0] *= gain;
			frame[1] *= gain;
		}
		frame[0] = soft_clip(frame[0], self.threshold);
		frame[1] = soft_clip(frame[1], self.threshold);
	}
}

/// Unchanged up to `threshold`, then bending smoothly towards 1.0.
#[inline]
fn soft_clip(x: f32, threshold: f32) -> f32 {
	let a = x.abs();
	if a <= threshold {
		return x;
	}
	let knee = 1.0 - threshold;
	if knee <= 0.0 {
		return x.signum();
	}
	(threshold + knee * ((a - threshold) / knee).tanh()).copysign(x)
}

/// Mixes the sources; lives on the thread that paces the audio. Control it
/// from anywhere through [`StreamMixer::handle`].
pub struct StreamMixer {
	shared: Arc<MixerShared>,
	sources: Vec<MixSource>,
	lane_buf: Vec<Frame>,
	source_buf: Vec<Frame>,
	mix_buf: Vec<Frame>,
	limiter: Limiter,
	gain: f32,
	meter: MeterState,
}

impl Default for StreamMixer {
	fn default() -> Self {
		Self::new(MixerConfig::default())
	}
}

impl StreamMixer {
	pub fn new(config: MixerConfig) -> Self {
		let config = MixerConfig { max_block: config.max_block.max(1), ..config };
		let block = config.max_block;
		let limiter = Limiter::new(&config);
		let shared = Arc::new(MixerShared {
			config,
			next_id: AtomicU64::new(1),
			sources: Mutex::new(Vec::new()),
			new_sources: Mutex::new(Vec::new()),
			has_new_sources: AtomicBool::new(false),
			gain: AtomicF32::new(1.0),
			meter: Meter::new(),
			limiter: AtomicF32::new(1.0),
			frames: AtomicU64::new(0),
		});
		Self {
			shared,
			sources: Vec::with_capacity(16),
			lane_buf: vec![SILENCE; block],
			source_buf: vec![SILENCE; block],
			mix_buf: vec![SILENCE; block],
			limiter,
			gain: 1.0,
			meter: MeterState::default(),
		}
	}

	pub fn handle(&self) -> MixerHandle {
		MixerHandle { shared: self.shared.clone() }
	}

	pub fn config(&self) -> &MixerConfig {
		&self.shared.config
	}

	/// Mix `out.len() / 2` frames of interleaved stereo into `out`
	/// (overwriting it), on the caller's clock.
	pub fn mix(&mut self, out: &mut [f32]) {
		let block = self.shared.config.max_block * 2;
		let even = out.len() & !1;
		let (frames, rest) = out.split_at_mut(even);
		for chunk in frames.chunks_mut(block) {
			self.mix_block(chunk);
		}
		rest.fill(0.0);
	}

	/// Pick up new sources and inputs, drop removed sources.
	fn adopt(&mut self) {
		if self.shared.has_new_sources.load(Ordering::Acquire)
			&& let Ok(mut new) = self.shared.new_sources.try_lock()
		{
			self.shared.has_new_sources.store(false, Ordering::Relaxed);
			for mut source in new.drain(..) {
				source.gain = source.shared.gain.load();
				source.mute = if source.shared.muted.load(Ordering::Relaxed) { 0.0 } else { 1.0 };
				self.sources.push(source);
			}
		}
		self.sources.retain(|s| !s.shared.removed.load(Ordering::Relaxed));
		for source in &mut self.sources {
			if source.shared.has_new_lanes.load(Ordering::Acquire)
				&& let Ok(mut new) = source.shared.new_lanes.try_lock()
			{
				source.shared.has_new_lanes.store(false, Ordering::Relaxed);
				source.lanes.append(&mut new);
			}
		}
	}

	fn mix_block(&mut self, out: &mut [f32]) {
		let n = out.len() / 2;
		self.adopt();
		let latency = self.shared.config.latency.as_secs_f64();
		let ballistics = Ballistics::new(n);
		let Self { sources, lane_buf, source_buf, mix_buf, .. } = self;
		let mix = &mut mix_buf[..n];
		mix.fill(SILENCE);
		for source in sources.iter_mut() {
			let sum = &mut source_buf[..n];
			let lane_out = &mut lane_buf[..n];
			let shared = &*source.shared;
			let mut any = false;
			let mut latency_seconds = 0.0f64;
			for lane in &mut source.lanes {
				let into = if any { &mut *lane_out } else { &mut *sum };
				if lane.render(into, latency, shared) {
					if any {
						for (s, l) in sum.iter_mut().zip(lane_out.iter()) {
							s[0] += l[0];
							s[1] += l[1];
						}
					}
					any = true;
				}
				if lane.playing {
					latency_seconds = latency_seconds.max(lane.fill / f64::from(lane.rate));
				}
			}
			source.lanes.retain(|l| !l.ended);
			if !source.lanes.is_empty() {
				source.had_lanes = true;
			}
			let state = if any {
				SourceState::Playing
			} else if source.lanes.is_empty() && source.had_lanes {
				SourceState::Ended
			} else {
				SourceState::Idle
			};
			shared.state.store(state as u8, Ordering::Relaxed);
			shared.inputs.store(source.lanes.len(), Ordering::Relaxed);
			shared.latency_us.store((latency_seconds * 1e6) as u64, Ordering::Relaxed);

			let gain = shared.gain.load();
			let mute = if shared.muted.load(Ordering::Relaxed) { 0.0 } else { 1.0 };
			if !any {
				source.meter.update(0.0, 0.0, 0, ballistics, &shared.meter);
				source.gain = gain;
				source.mute = mute;
				continue;
			}
			let (peak, squares) = add_source(sum, mix, (source.gain, gain), (source.mute, mute));
			source.meter.update(peak, squares, 2 * n, ballistics, &shared.meter);
			source.gain = gain;
			source.mute = mute;
		}

		// Master gain, limiter, meter, interleave.
		let gain = self.shared.gain.load();
		let from = self.gain;
		let ramp = (gain - from) / n as f32;
		self.limiter.lowest = 1.0;
		let (mut peak, mut squares) = (0.0f32, 0.0f32);
		for (k, (frame, pair)) in mix.iter_mut().zip(out.chunks_exact_mut(2)).enumerate() {
			let g = if ramp == 0.0 { gain } else { from + ramp * (k + 1) as f32 };
			if g != 1.0 {
				frame[0] *= g;
				frame[1] *= g;
			}
			self.limiter.process(frame);
			peak = peak.max(frame[0].abs()).max(frame[1].abs());
			squares += frame[0] * frame[0] + frame[1] * frame[1];
			pair[0] = frame[0];
			pair[1] = frame[1];
		}
		self.gain = gain;
		self.meter.update(peak, squares, 2 * n, ballistics, &self.shared.meter);
		self.shared.limiter.store(self.limiter.lowest);
		self.shared.frames.fetch_add(n as u64, Ordering::Relaxed);
	}
}

/// Add `sum` into `mix` with gain and mute ramped from `.0` to `.1` over
/// the block; returns the peak and sum of squares after the gain (before
/// the mute).
fn add_source(sum: &[Frame], mix: &mut [Frame], gain: (f32, f32), mute: (f32, f32)) -> (f32, f32) {
	let (mut peak, mut squares) = (0.0f32, 0.0f32);
	let n = sum.len() as f32;
	if gain.0 == gain.1 && mute.0 == mute.1 {
		let (g, m) = (gain.1, mute.1);
		for (s, o) in sum.iter().zip(mix.iter_mut()) {
			let (l, r) = (s[0] * g, s[1] * g);
			peak = peak.max(l.abs()).max(r.abs());
			squares += l * l + r * r;
			o[0] += l * m;
			o[1] += r * m;
		}
	} else {
		let dg = (gain.1 - gain.0) / n;
		let dm = (mute.1 - mute.0) / n;
		for (k, (s, o)) in sum.iter().zip(mix.iter_mut()).enumerate() {
			let t = (k + 1) as f32;
			let (g, m) = (gain.0 + dg * t, mute.0 + dm * t);
			let (l, r) = (s[0] * g, s[1] * g);
			peak = peak.max(l.abs()).max(r.abs());
			squares += l * l + r * r;
			o[0] += l * m;
			o[1] += r * m;
		}
	}
	(peak, squares)
}

impl Drop for StreamMixer {
	fn drop(&mut self) {
		// Inputs see the end (`SourceInput::is_closed`).
		for source in lock(&self.shared.sources).iter() {
			source.removed.store(true, Ordering::Relaxed);
		}
		for source in lock(&self.shared.new_sources).iter() {
			source.shared.removed.store(true, Ordering::Relaxed);
		}
	}
}

/// Controls a [`StreamMixer`] from any thread. Cheap to clone.
#[derive(Clone)]
pub struct MixerHandle {
	shared: Arc<MixerShared>,
}

impl MixerHandle {
	/// A new source (gain 1, not muted) with no inputs yet; the mixer picks
	/// it up at its next block.
	pub fn add_source(&self, name: impl Into<String>) -> SourceHandle {
		let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
		let shared = Arc::new(SourceShared {
			id,
			name: name.into(),
			gain: AtomicF32::new(1.0),
			muted: AtomicBool::new(false),
			removed: AtomicBool::new(false),
			meter: Meter::new(),
			state: AtomicU8::new(SourceState::Idle as u8),
			inputs: AtomicUsize::new(0),
			latency_us: AtomicU64::new(0),
			underruns: AtomicU64::new(0),
			overflows: AtomicU64::new(0),
			skipped: AtomicU64::new(0),
			new_lanes: Mutex::new(Vec::new()),
			has_new_lanes: AtomicBool::new(false),
		});
		let source = MixSource {
			shared: shared.clone(),
			lanes: Vec::with_capacity(4),
			gain: 1.0,
			mute: 1.0,
			had_lanes: false,
			meter: MeterState::default(),
		};
		lock(&self.shared.sources).push(shared.clone());
		lock(&self.shared.new_sources).push(source);
		self.shared.has_new_sources.store(true, Ordering::Release);
		SourceHandle { mixer: self.shared.clone(), shared }
	}

	/// The sources, in the order they were added.
	pub fn sources(&self) -> Vec<SourceHandle> {
		lock(&self.shared.sources)
			.iter()
			.map(|s| SourceHandle { mixer: self.shared.clone(), shared: s.clone() })
			.collect()
	}

	pub fn source(&self, id: u64) -> Option<SourceHandle> {
		lock(&self.shared.sources)
			.iter()
			.find(|s| s.id == id)
			.map(|s| SourceHandle { mixer: self.shared.clone(), shared: s.clone() })
	}

	/// Gain after the sum, before the limiter (linear; ramps over a block).
	pub fn set_gain(&self, gain: f32) {
		self.shared.gain.store(sanitize_gain(gain));
	}

	pub fn gain(&self) -> f32 {
		self.shared.gain.load()
	}

	/// Output level (after the limiter). Lock-free.
	pub fn level(&self) -> Level {
		self.shared.meter.load()
	}

	/// The limiter's lowest gain in the last block: 1 when it did nothing,
	/// 0.5 for 6 dB of reduction. Lock-free.
	pub fn limiter_gain(&self) -> f32 {
		self.shared.limiter.load()
	}

	/// Frames mixed so far.
	pub fn frames(&self) -> u64 {
		self.shared.frames.load(Ordering::Relaxed)
	}

	pub fn config(&self) -> &MixerConfig {
		&self.shared.config
	}
}

fn sanitize_gain(gain: f32) -> f32 {
	if gain.is_finite() { gain.max(0.0) } else { 1.0 }
}

/// Controls one source of a [`StreamMixer`]. Cheap to clone; the source
/// stays until [`remove`](Self::remove)d.
#[derive(Clone)]
pub struct SourceHandle {
	mixer: Arc<MixerShared>,
	shared: Arc<SourceShared>,
}

impl std::fmt::Debug for SourceHandle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SourceHandle")
			.field("id", &self.shared.id)
			.field("name", &self.shared.name)
			.finish()
	}
}

impl SourceHandle {
	pub fn id(&self) -> u64 {
		self.shared.id
	}

	pub fn name(&self) -> &str {
		&self.shared.name
	}

	/// Linear gain, any value >= 0 (1: unchanged); ramps over one block.
	pub fn set_gain(&self, gain: f32) {
		self.shared.gain.store(sanitize_gain(gain));
	}

	pub fn gain(&self) -> f32 {
		self.shared.gain.load()
	}

	/// Mute (ramped over one block). A muted source is still read and
	/// metered.
	pub fn set_muted(&self, muted: bool) {
		self.shared.muted.store(muted, Ordering::Relaxed);
	}

	pub fn muted(&self) -> bool {
		self.shared.muted.load(Ordering::Relaxed)
	}

	/// Level after the gain, before the mute. Lock-free.
	pub fn level(&self) -> Level {
		self.shared.meter.load()
	}

	pub fn state(&self) -> SourceState {
		SourceState::from_u8(self.shared.state.load(Ordering::Relaxed))
	}

	pub fn stats(&self) -> SourceStats {
		let s = &self.shared;
		SourceStats {
			state: self.state(),
			inputs: s.inputs.load(Ordering::Relaxed),
			latency: Duration::from_micros(s.latency_us.load(Ordering::Relaxed)),
			underruns: s.underruns.load(Ordering::Relaxed),
			overflows: s.overflows.load(Ordering::Relaxed),
			skipped: s.skipped.load(Ordering::Relaxed),
		}
	}

	/// A new input at `rate` Hz for a capture thread to push into. A source
	/// can have any number; each ends when its [`SourceInput`] is dropped
	/// and what it buffered has played.
	pub fn input(&self, rate: u32) -> SourceInput {
		let (lane, ring, lane_shared) = Lane::new(&self.mixer.config, rate);
		lock(&self.shared.new_lanes).push(lane);
		self.shared.has_new_lanes.store(true, Ordering::Release);
		SourceInput {
			ring,
			lane: lane_shared,
			source: self.shared.clone(),
			rate: rate.max(1),
			weights: Vec::new(),
		}
	}

	/// Take the source out of the mix (from the next block); its inputs see
	/// [`SourceInput::is_closed`].
	pub fn remove(&self) {
		self.shared.removed.store(true, Ordering::Relaxed);
		lock(&self.mixer.sources).retain(|s| s.id != self.shared.id);
		lock(&self.shared.new_lanes).clear();
	}

	pub fn is_removed(&self) -> bool {
		self.shared.removed.load(Ordering::Relaxed)
	}
}

/// The producing end of a source's input, for one capture thread. Pushing
/// never blocks or allocates (except a new channel count's downmix
/// weights); frames that do not fit in the ring are dropped and counted.
pub struct SourceInput {
	ring: rtrb::Producer<Frame>,
	lane: Arc<LaneShared>,
	source: Arc<SourceShared>,
	rate: u32,
	/// Downmix weights for more than two channels: (left, right) per
	/// channel.
	weights: Vec<Frame>,
}

impl std::fmt::Debug for SourceInput {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SourceInput")
			.field("source", &self.source.id)
			.field("rate", &self.rate)
			.finish()
	}
}

/// Non-finite samples become silence.
#[inline]
fn clean(x: f32) -> f32 {
	if x.is_finite() { x } else { 0.0 }
}

/// Stereo weights for `channels` > 2 in WAVE order (FL FR FC LFE BL BR
/// FLC FRC ...): front left and right as they are, centre into both at
/// -3 dB, LFE dropped, the rest alternately left and right at -3 dB. Four
/// channels are taken as quad (FL FR BL BR).
fn downmix_weights(channels: usize, weights: &mut Vec<Frame>) {
	const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
	weights.clear();
	weights.extend([[1.0, 0.0], [0.0, 1.0]]);
	let mut rest = 2;
	if channels == 3 || channels >= 5 {
		weights.push([H, H]);
		rest = 3;
	}
	if channels >= 6 {
		weights.push([0.0, 0.0]);
		rest = 4;
	}
	for i in rest..channels {
		weights.push(if (i - rest) % 2 == 0 { [H, 0.0] } else { [0.0, H] });
	}
}

impl SourceInput {
	pub fn rate(&self) -> u32 {
		self.rate
	}

	/// The source it feeds.
	pub fn source_id(&self) -> u64 {
		self.source.id
	}

	/// The source was removed or the mixer is gone: stop capturing.
	pub fn is_closed(&self) -> bool {
		self.source.removed.load(Ordering::Relaxed) || self.ring.is_abandoned()
	}

	/// Frames that fit now.
	pub fn free(&self) -> usize {
		self.ring.slots()
	}

	/// Push interleaved samples with `channels` channels (mono is played on
	/// both sides, more than two are downmixed). Returns the frames taken.
	pub fn push(&mut self, samples: &[f32], channels: u16) -> usize {
		match channels {
			0 => 0,
			1 => self.push_frames(samples.iter().map(|&s| [clean(s), clean(s)])),
			2 => self.push_frames(samples.chunks_exact(2).map(|f| [clean(f[0]), clean(f[1])])),
			n => {
				let n = usize::from(n);
				if self.weights.len() != n {
					downmix_weights(n, &mut self.weights);
				}
				let weights = std::mem::take(&mut self.weights);
				let taken = self.push_frames(samples.chunks_exact(n).map(|f| {
					let mut out = SILENCE;
					for (s, w) in f.iter().zip(&weights) {
						let s = clean(*s);
						out[0] += s * w[0];
						out[1] += s * w[1];
					}
					out
				}));
				self.weights = weights;
				taken
			}
		}
	}

	/// Push stereo frames. Returns the frames taken (the rest did not fit
	/// and is counted as overflow).
	pub fn push_frames<I>(&mut self, frames: I) -> usize
	where
		I: IntoIterator<Item = Frame>,
		I::IntoIter: ExactSizeIterator,
	{
		let frames = frames.into_iter();
		let len = frames.len();
		if len == 0 {
			return 0;
		}
		self.lane.max_push.fetch_max(len, Ordering::Relaxed);
		let n = len.min(self.ring.slots());
		let taken = match self.ring.write_chunk_uninit(n) {
			Ok(chunk) => chunk.fill_from_iter(frames),
			Err(_) => 0,
		};
		if taken < len {
			self.source.overflows.fetch_add((len - taken) as u64, Ordering::Relaxed);
		}
		taken
	}

	/// Push `frames` frames of silence (e.g. a muted microphone).
	pub fn push_silence(&mut self, frames: usize) -> usize {
		self.push_frames(std::iter::repeat_n(SILENCE, frames))
	}
}

/// Paces mixing on the monotonic clock: how many blocks of `frames` frames
/// are due. Block `k` is due `k * frames / 48000` s after the start,
/// computed exactly (no drift).
pub struct BlockClock {
	start: Instant,
	frames: u64,
	blocks: u64,
	/// After falling this many blocks behind (a suspended machine), skip
	/// ahead instead of mixing a burst.
	max_behind: u64,
}

impl BlockClock {
	pub fn new(frames: usize) -> Self {
		Self::starting_at(Instant::now(), frames)
	}

	pub fn starting_at(start: Instant, frames: usize) -> Self {
		Self { start, frames: frames.max(1) as u64, blocks: 0, max_behind: 25 }
	}

	fn at(&self, block: u64) -> Instant {
		let nanos =
			u128::from(block) * u128::from(self.frames) * 1_000_000_000 / u128::from(MIX_RATE);
		self.start + Duration::from_nanos(nanos as u64)
	}

	/// When the next block is due.
	pub fn next(&self) -> Instant {
		self.at(self.blocks)
	}

	/// Blocks due at `now` (0: wait until [`next`](Self::next)).
	pub fn due(&mut self, now: Instant) -> u64 {
		if now < self.next() {
			return 0;
		}
		let elapsed = now.duration_since(self.start).as_nanos();
		let reached =
			(elapsed * u128::from(MIX_RATE) / (u128::from(self.frames) * 1_000_000_000)) as u64 + 1;
		let due = reached - self.blocks;
		if due > self.max_behind {
			// Start over from now.
			self.start = now;
			self.blocks = 1;
			return 1;
		}
		self.blocks = reached;
		due
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const BLOCK: usize = 960;

	fn config() -> MixerConfig {
		MixerConfig { max_block: BLOCK, latency: Duration::from_millis(40), ..Default::default() }
	}

	/// Interleaved stereo DC.
	fn dc(frames: usize, left: f32, right: f32) -> Vec<f32> {
		(0..frames).flat_map(|_| [left, right]).collect()
	}

	fn sine(frames: usize, start: usize, rate: u32, freq: f64, amplitude: f32) -> Vec<f32> {
		(start..start + frames)
			.flat_map(|i| {
				let s = amplitude
					* (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin() as f32;
				[s, s]
			})
			.collect()
	}

	/// Goertzel magnitude of `freq` in the left channel, normalised so a
	/// full sine of amplitude `a` gives about `a / 2`.
	fn tone(samples: &[f32], freq: f64) -> f64 {
		let left: Vec<f64> = samples.chunks_exact(2).map(|f| f64::from(f[0])).collect();
		let k = 2.0 * (2.0 * std::f64::consts::PI * freq / f64::from(MIX_RATE)).cos();
		let (mut s1, mut s2) = (0.0, 0.0);
		for x in &left {
			let s0 = x + k * s1 - s2;
			s2 = s1;
			s1 = s0;
		}
		(s1 * s1 + s2 * s2 - k * s1 * s2).max(0.0).sqrt() / left.len() as f64
	}

	/// Run `blocks` blocks: before each, `feed` pushes for that block.
	fn run(mixer: &mut StreamMixer, blocks: usize, mut feed: impl FnMut(usize)) -> Vec<f32> {
		let mut out = Vec::new();
		let mut block = vec![0.0; BLOCK * 2];
		for b in 0..blocks {
			feed(b);
			mixer.mix(&mut block);
			out.extend_from_slice(&block);
		}
		out
	}

	#[test]
	fn sums_after_the_latency() {
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		let mut a = handle.add_source("a").input(MIX_RATE);
		let mut b = handle.add_source("b").input(MIX_RATE);
		let out = run(&mut mixer, 20, |_| {
			a.push(&dc(BLOCK, 0.25, 0.125), 2);
			b.push(&dc(BLOCK, 0.25, -0.5), 2);
		});
		// Silent while buffering (40 ms plus a block of headroom) ...
		assert!(out[..2 * BLOCK].iter().all(|&s| s == 0.0));
		// ... then exact sums (past the fade-in).
		let tail = &out[out.len() - 4 * BLOCK..];
		for f in tail.chunks_exact(2) {
			assert_eq!(f, [0.5, -0.375]);
		}
		for s in handle.sources() {
			assert_eq!(s.state(), SourceState::Playing);
			assert_eq!(s.stats().underruns, 0);
			assert!(s.stats().latency >= Duration::from_millis(40), "{:?}", s.stats());
		}
	}

	#[test]
	fn gains_and_mute_ramp_and_meter() {
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		let source = handle.add_source("a");
		source.set_gain(0.5);
		let mut input = source.input(MIX_RATE);
		let mut feed = |_| {
			input.push(&dc(BLOCK, 0.8, 0.8), 2);
		};
		let out = run(&mut mixer, 10, &mut feed);
		assert_eq!(out[out.len() - 2], 0.4);
		let level = source.level();
		// The RMS meter rises over about 300 ms towards 0.4.
		assert!((level.peak - 0.4).abs() < 1e-6 && (0.2..=0.4).contains(&level.rms), "{level:?}");
		assert!((handle.level().peak - 0.4).abs() < 1e-6);

		// A gain change ramps over one block, then holds.
		source.set_gain(1.0);
		let out = run(&mut mixer, 2, &mut feed);
		let first: Vec<f32> = out[..BLOCK * 2].iter().step_by(2).copied().collect();
		assert!(first.windows(2).all(|w| w[1] >= w[0]), "monotonic ramp");
		assert!((first[0] - 0.4).abs() < 0.01 && (first[BLOCK - 1] - 0.8).abs() < 1e-5);
		assert!(out[BLOCK * 2..].iter().all(|&s| s == 0.8));

		// Muted: silent output, the source meter still reads.
		source.set_muted(true);
		let out = run(&mut mixer, 3, &mut feed);
		assert!(out[BLOCK * 2..].iter().all(|&s| s == 0.0));
		assert!(source.level().peak > 0.79);
		assert!(source.muted());
		source.set_muted(false);
		let out = run(&mut mixer, 2, &mut feed);
		assert_eq!(out[out.len() - 1], 0.8);
	}

	#[test]
	fn master_gain_and_limiter() {
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		let mut a = handle.add_source("a").input(MIX_RATE);
		let mut b = handle.add_source("b").input(MIX_RATE);
		// Below the threshold nothing changes.
		let out = run(&mut mixer, 8, |b_| {
			a.push(&sine(BLOCK, b_ * BLOCK, MIX_RATE, 440.0, 0.4), 2);
			b.push(&sine(BLOCK, b_ * BLOCK, MIX_RATE, 440.0, 0.4), 2);
		});
		assert!(out.iter().all(|s| s.abs() <= 0.8 + 1e-6));
		assert_eq!(handle.limiter_gain(), 1.0);
		// 1.6 peak: limited below full scale, still loud and in tune.
		let out = run(&mut mixer, 20, |b_| {
			a.push(&sine(BLOCK, (b_ + 8) * BLOCK, MIX_RATE, 440.0, 0.8), 2);
			b.push(&sine(BLOCK, (b_ + 8) * BLOCK, MIX_RATE, 440.0, 0.8), 2);
		});
		let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
		assert!(peak <= 1.0, "{peak}");
		assert!(peak > 0.85, "{peak}");
		assert!(handle.limiter_gain() < 0.7, "{}", handle.limiter_gain());
		let tail = &out[out.len() - 8 * BLOCK..];
		assert!(tone(tail, 440.0) > 0.3, "{}", tone(tail, 440.0));
		// The master gain applies before the limiter.
		handle.set_gain(0.25);
		let out = run(&mut mixer, 6, |b_| {
			a.push(&dc(BLOCK, 0.4, 0.4), 2);
			b.push(&dc(BLOCK, 0.4, 0.4), 2);
			let _ = b_;
		});
		assert!((out[out.len() - 1] - 0.2).abs() < 1e-6, "{}", out[out.len() - 1]);
		assert!(soft_clip(10.0, 0.891) <= 1.0 && soft_clip(-10.0, 0.891) >= -1.0);
		assert_eq!(soft_clip(0.5, 0.891), 0.5);
	}

	#[test]
	fn resamples_and_maps_channels() {
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		// 44.1 kHz mono 1 kHz tone, 10 ms pushes.
		let mut mono = handle.add_source("mono").input(44_100);
		let mut n = 0;
		let out = run(&mut mixer, 100, |_| {
			for _ in 0..2 {
				let block: Vec<f32> = (n..n + 441)
					.map(|i| {
						0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44_100.0).sin()
							as f32
					})
					.collect();
				n += 441;
				mono.push(&block, 1);
			}
		});
		let tail = &out[out.len() - 50 * BLOCK * 2..];
		let at = tone(tail, 1000.0);
		assert!(at > 0.2, "{at}");
		assert!(tone(tail, 1100.0) < at / 50.0);
		// Both channels.
		assert!(tail.chunks_exact(2).all(|f| f[0] == f[1]));
		// Output keeps pace with input: nothing ran dry or piled up.
		let stats = handle.sources()[0].stats();
		assert_eq!((stats.underruns, stats.skipped, stats.overflows), (0, 0, 0), "{stats:?}");

		// 5.1 downmix: centre into both sides, LFE dropped.
		let mut weights = Vec::new();
		downmix_weights(6, &mut weights);
		assert_eq!(weights.len(), 6);
		assert_eq!(weights[3], [0.0, 0.0]);
		assert!(weights[2][0] > 0.7 && weights[2][1] > 0.7);
		assert_eq!((weights[4][1], weights[5][0]), (0.0, 0.0));
		let mut mixer = StreamMixer::new(config());
		let mut six = mixer.handle().add_source("5.1").input(MIX_RATE);
		let frame = [0.1, 0.2, 0.0, 1.0, 0.0, 0.0];
		let out = run(&mut mixer, 6, |_| {
			let samples: Vec<f32> = frame.iter().copied().cycle().take(BLOCK * 6).collect();
			six.push(&samples, 6);
		});
		assert_eq!(&out[out.len() - 2..], [0.1, 0.2]);
	}

	#[test]
	fn a_stalled_or_ended_source_does_not_hold_up_others() {
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		let steady = handle.add_source("steady");
		let flaky = handle.add_source("flaky");
		let mut a = steady.input(MIX_RATE);
		let mut b = flaky.input(MIX_RATE);
		let mut feed = |block: usize, flaky_on: bool| {
			a.push(&dc(BLOCK, 0.25, 0.25), 2);
			if flaky_on {
				b.push(&dc(BLOCK, 0.5, 0.5), 2);
			}
			let _ = block;
		};
		let out = run(&mut mixer, 10, |b_| feed(b_, true));
		assert_eq!(out[out.len() - 1], 0.75);
		// The flaky source stops: the steady one goes on alone.
		let out = run(&mut mixer, 10, |b_| feed(b_, false));
		assert_eq!(out[out.len() - 1], 0.25);
		assert_eq!(flaky.state(), SourceState::Idle);
		assert_eq!(steady.state(), SourceState::Playing);
		assert_eq!(steady.stats().underruns, 0);
		// It comes back after buffering its latency.
		let out = run(&mut mixer, 10, |b_| feed(b_, true));
		assert_eq!(out[out.len() - 1], 0.75);
		assert_eq!(flaky.stats().underruns, 1);
		// Its capture ends: what was buffered plays out, then it is ended.
		drop(b);
		let out = run(&mut mixer, 10, |_| {
			a.push(&dc(BLOCK, 0.25, 0.25), 2);
		});
		assert_eq!(out[out.len() - 1], 0.25);
		assert_eq!(flaky.state(), SourceState::Ended);
		assert_eq!(flaky.stats().inputs, 0);
		assert_eq!(flaky.stats().underruns, 1, "an end is not an underrun");
	}

	#[test]
	fn sources_come_and_go_while_mixing() {
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		let a = handle.add_source("a");
		let mut ia = a.input(MIX_RATE);
		let out = run(&mut mixer, 6, |_| {
			ia.push(&dc(BLOCK, 0.1, 0.1), 2);
		});
		assert_eq!(out[out.len() - 1], 0.1);
		// Any number of sources, added live.
		let many: Vec<(SourceHandle, SourceInput)> = (0..40)
			.map(|i| {
				let s = handle.add_source(format!("s{i}"));
				s.set_gain(0.5);
				let input = s.input(if i % 2 == 0 { MIX_RATE } else { 44_100 });
				(s, input)
			})
			.collect();
		let mut many: Vec<_> = many;
		let out = run(&mut mixer, 10, |_| {
			ia.push(&dc(BLOCK, 0.1, 0.1), 2);
			for (_, input) in many.iter_mut() {
				let frames = if input.rate() == MIX_RATE { BLOCK } else { 882 };
				input.push(&dc(frames, 0.01, 0.01), 2);
			}
		});
		let expected = 0.1 + 40.0 * 0.005;
		assert!((out[out.len() - 1] - expected).abs() < 1e-4, "{}", out[out.len() - 1]);
		assert_eq!(handle.sources().len(), 41);
		// Removed ones leave at the next block; their inputs see it.
		for (s, _) in &many {
			s.remove();
		}
		assert!(many.iter().all(|(_, input)| input.is_closed()));
		let out = run(&mut mixer, 2, |_| {
			ia.push(&dc(BLOCK, 0.1, 0.1), 2);
		});
		assert_eq!(out[out.len() - 1], 0.1);
		assert_eq!(handle.sources().len(), 1);
		// A second input on a live source adds to it.
		let mut ib = a.input(MIX_RATE);
		let out = run(&mut mixer, 8, |_| {
			ia.push(&dc(BLOCK, 0.1, 0.1), 2);
			ib.push(&dc(BLOCK, 0.2, 0.2), 2);
		});
		assert!((out[out.len() - 1] - 0.3).abs() < 1e-6);
		assert_eq!(a.stats().inputs, 2);
		assert!(handle.source(a.id()).is_some());
		drop(mixer);
		assert!(ia.is_closed());
	}

	#[test]
	fn drift_is_absorbed() {
		// A source whose clock runs 0.1 % fast and one 0.1 % slow, for five
		// simulated minutes: no skips, no underruns, and the latency stays
		// where it settled, give or take the correction band.
		for ppm in [1000.0, -1000.0] {
			let mut mixer = StreamMixer::new(config());
			let handle = mixer.handle();
			let source = handle.add_source("drift");
			let mut input = source.input(MIX_RATE);
			let per_block = BLOCK as f64 * (1.0 + ppm / 1e6);
			let (mut owed, mut phase) = (0.0, 0usize);
			let mut block = vec![0.0; BLOCK * 2];
			let mut settled = None;
			let mut worst = 0.0f64;
			for b in 0..15_000 {
				owed += per_block;
				let frames = owed as usize;
				owed -= frames as f64;
				input.push(&sine(frames, phase, MIX_RATE, 300.0, 0.3), 2);
				phase += frames;
				mixer.mix(&mut block);
				let latency = source.stats().latency.as_secs_f64();
				if b == 200 {
					settled = Some(latency);
				}
				if let Some(settled) = settled {
					worst = worst.max((latency - settled).abs());
				}
			}
			let stats = source.stats();
			assert_eq!(
				(stats.underruns, stats.skipped, stats.overflows),
				(0, 0, 0),
				"{ppm}: {stats:?}"
			);
			// Uncorrected, 0.1 % is 300 ms after five minutes.
			assert!(worst < 0.015, "{ppm}: latency moved by {worst} s");
			assert!(tone(&block, 300.0) > 0.1);
		}
	}

	#[test]
	fn bursty_input_gets_more_latency() {
		// 8192-frame bursts every 170 ms (a power-saving PipeWire quantum).
		let mut mixer = StreamMixer::new(config());
		let handle = mixer.handle();
		let source = handle.add_source("bursty");
		let mut input = source.input(MIX_RATE);
		let mut owed = 0usize;
		let mut phase = 0;
		let mut block = vec![0.0; BLOCK * 2];
		for _ in 0..500 {
			owed += BLOCK;
			if owed >= 8192 {
				input.push(&sine(8192, phase, MIX_RATE, 500.0, 0.3), 2);
				phase += 8192;
				owed -= 8192;
			}
			mixer.mix(&mut block);
		}
		let stats = source.stats();
		assert!(stats.latency > Duration::from_millis(170), "{stats:?}");
		// At most the first burst ran short before the latency adapted.
		assert!(stats.underruns <= 1, "{stats:?}");
		assert_eq!(source.state(), SourceState::Playing);
	}

	#[test]
	fn nan_and_short_calls() {
		let mut mixer = StreamMixer::new(config());
		let mut input = mixer.handle().add_source("x").input(MIX_RATE);
		let mut samples = dc(BLOCK * 4, 0.5, 0.5);
		samples[10] = f32::NAN;
		samples[11] = f32::INFINITY;
		input.push(&samples, 2);
		// Odd lengths and blocks longer than `max_block` are fine.
		let mut out = vec![1.0; BLOCK * 3 + 1];
		mixer.mix(&mut out);
		assert!(out.iter().all(|s| s.is_finite()));
		assert_eq!(out[out.len() - 1], 0.0);
		assert_eq!(input.push(&[], 2), 0);
		assert_eq!(input.push(&[0.1], 0), 0);
	}

	#[test]
	fn cubic_is_exact_on_samples_and_lines() {
		assert_eq!(cubic(0.3, -0.7, 0.2, 0.9, 0.0), -0.7);
		// A straight line is reproduced.
		let v = cubic(0.0, 1.0, 2.0, 3.0, 0.25);
		assert!((v - 1.25).abs() < 1e-6, "{v}");
	}

	#[test]
	fn block_clock_paces_and_skips_ahead() {
		let start = Instant::now();
		let mut clock = BlockClock::starting_at(start, 960);
		assert_eq!(clock.due(start), 1);
		assert_eq!(clock.due(start), 0);
		assert_eq!(clock.next(), start + Duration::from_millis(20));
		assert_eq!(clock.due(start + Duration::from_millis(59)), 2);
		assert_eq!(clock.due(start + Duration::from_millis(60)), 1);
		// Exact over a long run: 441 frames is 9.1875 ms.
		let mut clock = BlockClock::starting_at(start, 441);
		let mut total = 0;
		for ms in 1..=10_000u64 {
			total += clock.due(start + Duration::from_millis(ms));
		}
		assert_eq!(total, 10_000 * 48 / 441 + 1);
		// After a long stall it starts over instead of bursting.
		let mut clock = BlockClock::starting_at(start, 960);
		clock.due(start);
		assert_eq!(clock.due(start + Duration::from_secs(10)), 1);
		assert_eq!(clock.due(start + Duration::from_millis(10_020)), 1);
		assert!(to_db(1.0).abs() < 1e-6 && to_db(0.0) == -120.0);
	}
}
