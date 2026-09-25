//! Audio capture and playback through cpal.
//!
//! The device callbacks only move samples through lock-free ring buffers; all
//! processing happens on the caller's side.
//!
//! Devices are identified by cpal's stable device id (`host:device`, e.g.
//! `alsa:hw:1,0`), which survives restarts and replugging. A stream notices
//! when its device goes away through cpal's error callback
//! ([`Capture::is_lost`]); [`Managed`] reopens it, on the default device when
//! the selected one is gone, and moves back when it returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, SampleFormat};
use rtrb::{Consumer, Producer, RingBuffer};
use tracing::{debug, warn};

use crate::{Error, Result};

/// A device as shown in settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
	/// Stable id to store in settings (`AudioSettings::input_device`).
	pub id: String,
	/// Human-readable name.
	pub name: String,
	pub input: bool,
	pub output: bool,
	/// The system's default input device.
	pub default_input: bool,
	/// The system's default output device.
	pub default_output: bool,
}

fn device_error(e: impl std::fmt::Display) -> Error {
	Error::Device(e.to_string())
}

fn device_name(device: &cpal::Device) -> String {
	device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| device.to_string())
}

fn device_id(device: &cpal::Device) -> Option<String> {
	device.id().ok().map(|id| id.to_string())
}

/// All devices of the default host.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
	let host = cpal::default_host();
	let default_input = host.default_input_device().and_then(|d| device_id(&d));
	let default_output = host.default_output_device().and_then(|d| device_id(&d));
	let devices = host.devices().map_err(device_error)?;
	Ok(devices
		.filter_map(|d| {
			let id = device_id(&d)?;
			Some(DeviceInfo {
				name: device_name(&d),
				input: d.supports_input(),
				output: d.supports_output(),
				default_input: default_input.as_deref() == Some(id.as_str()),
				default_output: default_output.as_deref() == Some(id.as_str()),
				id,
			})
		})
		.collect())
}

/// Devices that can record.
pub fn input_devices() -> Result<Vec<DeviceInfo>> {
	Ok(list_devices()?.into_iter().filter(|d| d.input).collect())
}

/// Devices that can play.
pub fn output_devices() -> Result<Vec<DeviceInfo>> {
	Ok(list_devices()?.into_iter().filter(|d| d.output).collect())
}

/// The device with this id, or the default one for `None`.
fn find_device(id: Option<&str>, input: bool) -> Result<cpal::Device> {
	let host = cpal::default_host();
	let kind = if input { "input" } else { "output" };
	let Some(id) = id else {
		let device = if input { host.default_input_device() } else { host.default_output_device() };
		return device.ok_or_else(|| Error::Device(format!("no {kind} device")));
	};
	let parsed = id.parse().map_err(device_error)?;
	host.device_by_id(&parsed).ok_or_else(|| Error::Device(format!("{kind} device {id} not found")))
}

/// Whether a device with this id is present.
pub fn device_exists(id: &str) -> bool {
	id.parse().ok().and_then(|id| cpal::default_host().device_by_id(&id)).is_some()
}

/// Errors after which a stream will not recover by itself.
fn is_fatal(kind: ErrorKind) -> bool {
	!matches!(kind, ErrorKind::DeviceChanged | ErrorKind::Xrun | ErrorKind::RealtimeDenied)
}

/// The error callback: log, and flag the stream lost on fatal errors.
fn error_callback(what: &'static str, lost: Arc<AtomicBool>) -> impl FnMut(cpal::Error) + Send {
	move |error| {
		if is_fatal(error.kind()) {
			warn!(%error, "{what} stream lost");
			lost.store(true, Ordering::Relaxed);
		} else {
			debug!(%error, "{what} stream");
		}
	}
}

/// The default config, but with f32 samples if the device offers them at the
/// same rate (streams here are f32 only).
fn f32_config(
	default: cpal::SupportedStreamConfig,
	supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> cpal::StreamConfig {
	if default.sample_format() == SampleFormat::F32 {
		return default.config();
	}
	let rate = default.sample_rate();
	supported
		.filter(|r| r.sample_format() == SampleFormat::F32)
		.find_map(|r| r.try_with_sample_rate(rate))
		.map(|c| c.config())
		.unwrap_or_else(|| default.config())
}

/// Running capture stream. Samples are interleaved at the device's rate.
pub struct Capture {
	_stream: cpal::Stream,
	consumer: Consumer<f32>,
	lost: Arc<AtomicBool>,
	id: Option<String>,
	name: String,
	pub channels: usize,
	pub rate: u32,
}

impl Capture {
	/// Open the default input device, buffering up to `buffer_ms` of audio.
	pub fn open_default(buffer_ms: u32) -> Result<Self> {
		Self::open(None, buffer_ms)
	}

	/// Open the input device with this id (see [`DeviceInfo::id`]), or the
	/// default one for `None`.
	pub fn open(id: Option<&str>, buffer_ms: u32) -> Result<Self> {
		let device = find_device(id, true)?;
		let default = device.default_input_config().map_err(device_error)?;
		let supported = device.supported_input_configs().map_err(device_error)?;
		let config = f32_config(default, supported);
		let channels = config.channels as usize;
		let rate = config.sample_rate;
		let (mut producer, consumer) =
			RingBuffer::new((rate * buffer_ms / 1000) as usize * channels);
		let lost = Arc::new(AtomicBool::new(false));
		let stream = device
			.build_input_stream(
				config,
				move |data: &[f32], _: &cpal::InputCallbackInfo| {
					let n = producer.slots().min(data.len());
					if let Ok(chunk) = producer.write_chunk_uninit(n) {
						chunk.fill_from_iter(data[..n].iter().copied());
					}
				},
				error_callback("capture", lost.clone()),
				None,
			)
			.map_err(device_error)?;
		stream.play().map_err(device_error)?;
		Ok(Self {
			_stream: stream,
			consumer,
			lost,
			id: device_id(&device),
			name: device_name(&device),
			channels,
			rate,
		})
	}

	/// Move everything captured so far into `out`.
	pub fn read_available(&mut self, out: &mut Vec<f32>) {
		let n = self.consumer.slots();
		if let Ok(chunk) = self.consumer.read_chunk(n) {
			out.extend(chunk);
		}
	}

	/// The device went away or the stream broke; reopen it.
	pub fn is_lost(&self) -> bool {
		self.lost.load(Ordering::Relaxed)
	}

	pub fn name(&self) -> &str {
		&self.name
	}
}

/// Running playback stream. Write interleaved samples at the device's rate.
pub struct Playback {
	_stream: cpal::Stream,
	producer: Producer<f32>,
	lost: Arc<AtomicBool>,
	id: Option<String>,
	name: String,
	capacity: usize,
	pub channels: usize,
	pub rate: u32,
}

impl Playback {
	pub fn open_default(buffer_ms: u32) -> Result<Self> {
		Self::open(None, buffer_ms)
	}

	/// Open the output device with this id, or the default one for `None`.
	pub fn open(id: Option<&str>, buffer_ms: u32) -> Result<Self> {
		let device = find_device(id, false)?;
		let default = device.default_output_config().map_err(device_error)?;
		let supported = device.supported_output_configs().map_err(device_error)?;
		let config = f32_config(default, supported);
		let channels = config.channels as usize;
		let rate = config.sample_rate;
		let capacity = (rate * buffer_ms / 1000) as usize * channels;
		let (producer, mut consumer) = RingBuffer::<f32>::new(capacity);
		let lost = Arc::new(AtomicBool::new(false));
		let stream = device
			.build_output_stream(
				config,
				move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
					let n = consumer.slots().min(data.len());
					if let Ok(chunk) = consumer.read_chunk(n) {
						for (dst, src) in data.iter_mut().zip(chunk) {
							*dst = src;
						}
					}
					// Underrun: play silence.
					data[n..].fill(0.0);
				},
				error_callback("playback", lost.clone()),
				None,
			)
			.map_err(device_error)?;
		stream.play().map_err(device_error)?;
		Ok(Self {
			_stream: stream,
			producer,
			lost,
			id: device_id(&device),
			name: device_name(&device),
			capacity,
			channels,
			rate,
		})
	}

	/// Queue samples; returns how many fit into the buffer.
	pub fn write(&mut self, samples: &[f32]) -> usize {
		let n = self.producer.slots().min(samples.len());
		if let Ok(chunk) = self.producer.write_chunk_uninit(n) {
			chunk.fill_from_iter(samples[..n].iter().copied());
		}
		n
	}

	/// Samples that can be written without blocking.
	pub fn free(&self) -> usize {
		self.producer.slots()
	}

	/// Samples (all channels) queued and not yet handed to the device.
	pub fn queued(&self) -> usize {
		self.capacity - self.producer.slots()
	}

	/// Queued audio in milliseconds.
	pub fn queued_ms(&self) -> u32 {
		(self.queued() / self.channels.max(1)) as u32 * 1000 / self.rate.max(1)
	}

	/// The device went away or the stream broke; reopen it.
	pub fn is_lost(&self) -> bool {
		self.lost.load(Ordering::Relaxed)
	}

	pub fn name(&self) -> &str {
		&self.name
	}
}

/// What [`Managed`] needs from a stream; implemented by [`Capture`] and
/// [`Playback`].
pub trait DeviceStream: Sized {
	fn open(id: Option<&str>, buffer_ms: u32) -> Result<Self>;
	fn is_lost(&self) -> bool;
	/// Id of the device actually opened.
	fn device_id(&self) -> Option<&str>;
	fn name(&self) -> &str;
	fn device_exists(id: &str) -> bool;
}

impl DeviceStream for Capture {
	fn open(id: Option<&str>, buffer_ms: u32) -> Result<Self> {
		Capture::open(id, buffer_ms)
	}
	fn is_lost(&self) -> bool {
		Capture::is_lost(self)
	}
	fn device_id(&self) -> Option<&str> {
		self.id.as_deref()
	}
	fn name(&self) -> &str {
		&self.name
	}
	fn device_exists(id: &str) -> bool {
		device_exists(id)
	}
}

impl DeviceStream for Playback {
	fn open(id: Option<&str>, buffer_ms: u32) -> Result<Self> {
		Playback::open(id, buffer_ms)
	}
	fn is_lost(&self) -> bool {
		Playback::is_lost(self)
	}
	fn device_id(&self) -> Option<&str> {
		self.id.as_deref()
	}
	fn name(&self) -> &str {
		&self.name
	}
	fn device_exists(id: &str) -> bool {
		device_exists(id)
	}
}

/// What happened to a [`Managed`] stream, for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
	/// Opened a device. `fallback` is set when it is the default device
	/// because the selected one is missing.
	Opened { name: String, fallback: bool },
	/// The open device went away.
	Lost { name: String },
	/// No device could be opened; retrying in the background. Reported once
	/// until a device opens again.
	Unavailable(String),
}

/// Retry opening a device this often while none is open.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// While on the default device instead of the selected one, look for the
/// selected one this often.
pub const RETURN_INTERVAL: Duration = Duration::from_secs(5);

/// Keeps a stream open across device loss and selection changes. Call
/// [`poll`](Self::poll) regularly from the audio loop.
pub struct Managed<S> {
	stream: Option<S>,
	wanted: Option<String>,
	buffer_ms: u32,
	retry_at: Instant,
	return_at: Instant,
	failing: bool,
}

impl<S: DeviceStream> Managed<S> {
	/// Nothing is opened until the first [`poll`](Self::poll).
	pub fn new(wanted: Option<String>, buffer_ms: u32) -> Self {
		let now = Instant::now();
		Self { stream: None, wanted, buffer_ms, retry_at: now, return_at: now, failing: false }
	}

	/// Select another device (`None`: the default); reopens on the next poll.
	pub fn select(&mut self, wanted: Option<String>) {
		if wanted != self.wanted {
			self.wanted = wanted;
			self.stream = None;
			self.retry_at = Instant::now();
		}
	}

	pub fn wanted(&self) -> Option<&str> {
		self.wanted.as_deref()
	}

	pub fn stream(&mut self) -> Option<&mut S> {
		self.stream.as_mut()
	}

	/// Whether the stream was replaced since the caller last saw it matters
	/// for resamplers; callers compare [`DeviceEvent::Opened`].
	pub fn poll(&mut self) -> Option<DeviceEvent> {
		self.poll_at(Instant::now())
	}

	fn poll_at(&mut self, now: Instant) -> Option<DeviceEvent> {
		if let Some(stream) = &self.stream {
			if stream.is_lost() {
				let name = stream.name().to_string();
				self.stream = None;
				self.retry_at = now;
				return Some(DeviceEvent::Lost { name });
			}
			// On the default device although another one is selected: move
			// back once it is present again.
			let on_fallback = self.wanted.is_some() && stream.device_id() != self.wanted.as_deref();
			if !on_fallback || now < self.return_at {
				return None;
			}
			self.return_at = now + RETURN_INTERVAL;
			match self.wanted.as_deref() {
				Some(id) if S::device_exists(id) => self.stream = None,
				_ => return None,
			}
		}
		if now < self.retry_at {
			return None;
		}
		let wanted = self.wanted.clone();
		let opened = match S::open(wanted.as_deref(), self.buffer_ms) {
			Ok(s) => Ok((s, false)),
			Err(e) if wanted.is_some() => {
				S::open(None, self.buffer_ms).map(|s| (s, true)).map_err(|default_error| {
					Error::Device(format!("{e}; default device: {default_error}"))
				})
			}
			Err(e) => Err(e),
		};
		match opened {
			Ok((stream, fallback)) => {
				let name = stream.name().to_string();
				self.stream = Some(stream);
				self.failing = false;
				self.return_at = now + RETURN_INTERVAL;
				Some(DeviceEvent::Opened { name, fallback })
			}
			Err(e) => {
				self.retry_at = now + RETRY_INTERVAL;
				let first = !self.failing;
				self.failing = true;
				first.then(|| DeviceEvent::Unavailable(e.to_string()))
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::cell::RefCell;
	use std::collections::HashSet;

	use super::*;

	thread_local! {
		/// Plugged-in fake devices; "default" is the system default.
		static PLUGGED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
	}

	fn plug(id: &str, on: bool) {
		PLUGGED.with(|p| {
			let mut p = p.borrow_mut();
			if on { p.insert(id.into()) } else { p.remove(id) }
		});
	}

	fn plugged(id: &str) -> bool {
		PLUGGED.with(|p| p.borrow().contains(id))
	}

	struct Fake {
		id: String,
	}

	impl DeviceStream for Fake {
		fn open(id: Option<&str>, _: u32) -> Result<Self> {
			let id = id.unwrap_or("default");
			if plugged(id) {
				Ok(Fake { id: id.into() })
			} else {
				Err(Error::Device(format!("{id} not found")))
			}
		}
		fn is_lost(&self) -> bool {
			!plugged(&self.id)
		}
		fn device_id(&self) -> Option<&str> {
			Some(&self.id)
		}
		fn name(&self) -> &str {
			&self.id
		}
		fn device_exists(id: &str) -> bool {
			plugged(id)
		}
	}

	fn opened(name: &str, fallback: bool) -> Option<DeviceEvent> {
		Some(DeviceEvent::Opened { name: name.into(), fallback })
	}

	#[test]
	fn fatal_errors() {
		assert!(is_fatal(ErrorKind::DeviceNotAvailable));
		assert!(is_fatal(ErrorKind::StreamInvalidated));
		assert!(!is_fatal(ErrorKind::Xrun));
		assert!(!is_fatal(ErrorKind::DeviceChanged));
	}

	/// Headset selected, unplugged mid-call, plugged back in.
	#[test]
	fn reopens_and_falls_back() {
		plug("default", true);
		plug("headset", true);
		let mut m = Managed::<Fake>::new(Some("headset".into()), 100);
		let t0 = Instant::now();
		assert_eq!(m.poll_at(t0), opened("headset", false));
		assert_eq!(m.poll_at(t0), None);

		plug("headset", false);
		assert_eq!(m.poll_at(t0), Some(DeviceEvent::Lost { name: "headset".into() }));
		assert_eq!(m.poll_at(t0), opened("default", true));
		assert_eq!(m.stream().unwrap().id, "default");

		// Back only after the return interval.
		plug("headset", true);
		assert_eq!(m.poll_at(t0 + Duration::from_secs(1)), None);
		assert_eq!(m.poll_at(t0 + RETURN_INTERVAL), opened("headset", false));

		// Selecting the default device switches right away.
		m.select(None);
		assert_eq!(m.poll_at(t0 + RETURN_INTERVAL), opened("default", false));
	}

	/// Nothing plugged in: one report, retries every interval, recovers.
	#[test]
	fn retries_when_unavailable() {
		plug("default", false);
		let mut m = Managed::<Fake>::new(None, 100);
		let t0 = Instant::now();
		assert!(matches!(m.poll_at(t0), Some(DeviceEvent::Unavailable(_))));
		assert!(m.stream().is_none());
		plug("default", true);
		// Not before the retry interval, and no repeated report.
		assert_eq!(m.poll_at(t0 + Duration::from_millis(500)), None);
		assert_eq!(m.poll_at(t0 + RETRY_INTERVAL), opened("default", false));
		plug("default", false);
		assert!(matches!(m.poll_at(t0 + RETRY_INTERVAL), Some(DeviceEvent::Lost { .. })));
		assert!(matches!(m.poll_at(t0 + RETRY_INTERVAL), Some(DeviceEvent::Unavailable(_))));
		assert_eq!(m.poll_at(t0 + RETRY_INTERVAL * 2), None);
	}

	/// Opening real devices needs hardware; just make sure enumeration does
	/// not fail or panic without any.
	#[test]
	fn enumeration_without_hardware() {
		if let Ok(devices) = list_devices() {
			for d in devices {
				assert!(!d.id.is_empty());
			}
		}
		assert!(!device_exists("no-such-host:nothing"));
	}
}
