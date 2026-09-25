//! Audio capture and playback through cpal.
//!
//! The device callbacks only move samples through lock-free ring buffers; all
//! processing happens on the caller's side.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};
use tracing::warn;

use crate::{Error, Result};

/// A device as shown in settings.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
	pub name: String,
	pub input: bool,
	pub output: bool,
}

fn device_error(e: impl std::fmt::Display) -> Error {
	Error::Device(e.to_string())
}

/// All devices of the default host.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
	let host = cpal::default_host();
	let devices = host.devices().map_err(device_error)?;
	Ok(devices
		.map(|d| DeviceInfo {
			name: d.description().map(|d| d.name().to_string()).unwrap_or_default(),
			input: d.supports_input(),
			output: d.supports_output(),
		})
		.collect())
}

/// Running capture stream. Samples are interleaved at the device's rate.
pub struct Capture {
	_stream: cpal::Stream,
	consumer: Consumer<f32>,
	pub channels: usize,
	pub rate: u32,
}

impl Capture {
	/// Open the default input device, buffering up to `buffer_ms` of audio.
	pub fn open_default(buffer_ms: u32) -> Result<Self> {
		let device = cpal::default_host()
			.default_input_device()
			.ok_or_else(|| Error::Device("no input device".into()))?;
		let config = device.default_input_config().map_err(device_error)?.config();
		let channels = config.channels as usize;
		let rate = config.sample_rate;
		let (mut producer, consumer) =
			RingBuffer::new((rate * buffer_ms / 1000) as usize * channels);
		let stream = device
			.build_input_stream(
				config,
				move |data: &[f32], _: &cpal::InputCallbackInfo| {
					let n = producer.slots().min(data.len());
					if let Ok(chunk) = producer.write_chunk_uninit(n) {
						chunk.fill_from_iter(data[..n].iter().copied());
					}
				},
				|error| warn!(%error, "capture stream error"),
				None,
			)
			.map_err(device_error)?;
		stream.play().map_err(device_error)?;
		Ok(Self { _stream: stream, consumer, channels, rate })
	}

	/// Move everything captured so far into `out`.
	pub fn read_available(&mut self, out: &mut Vec<f32>) {
		let n = self.consumer.slots();
		if let Ok(chunk) = self.consumer.read_chunk(n) {
			out.extend(chunk);
		}
	}
}

/// Running playback stream. Write interleaved samples at the device's rate.
pub struct Playback {
	_stream: cpal::Stream,
	producer: Producer<f32>,
	pub channels: usize,
	pub rate: u32,
}

impl Playback {
	pub fn open_default(buffer_ms: u32) -> Result<Self> {
		let device = cpal::default_host()
			.default_output_device()
			.ok_or_else(|| Error::Device("no output device".into()))?;
		let config = device.default_output_config().map_err(device_error)?.config();
		let channels = config.channels as usize;
		let rate = config.sample_rate;
		let (producer, mut consumer) =
			RingBuffer::<f32>::new((rate * buffer_ms / 1000) as usize * channels);
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
				|error| warn!(%error, "playback stream error"),
				None,
			)
			.map_err(device_error)?;
		stream.play().map_err(device_error)?;
		Ok(Self { _stream: stream, producer, channels, rate })
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
}
