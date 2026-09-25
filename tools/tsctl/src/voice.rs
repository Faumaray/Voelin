//! `tsctl connect ... voice send|record`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time::Instant;
use tsc_audio::pcm::{self, FRAME_SAMPLES, SAMPLE_RATE};
use tsc_audio::{Framer, Mixer, VoiceCodec, VoiceEncoder, resample, wav};
use tsclientlib::{ClientId, Connection, StreamItem};
use tsproto_packets::packets::AudioData;

use crate::VoiceCommand;
use crate::session::{Flow, PumpEnd, pump};

const FRAME: Duration = Duration::from_millis(20);

pub async fn run(con: &mut Connection, command: &VoiceCommand) -> Result<()> {
	match command {
		VoiceCommand::Send { file, tone, seconds, music } => {
			let codec = if *music { VoiceCodec::Music } else { VoiceCodec::Voice };
			let samples = match (file, tone) {
				(Some(path), _) => load(path, codec.channels())?,
				(None, Some(freq)) => {
					pcm::from_mono(&pcm::sine(*freq, *seconds, 0.5), codec.channels())
				}
				(None, None) => bail!("pass a WAV file or --tone <hz>"),
			};
			send(con, codec, &samples).await
		}
		VoiceCommand::Record { out, seconds, expect_tone } => {
			let samples = record(con, Duration::from_secs_f32(*seconds)).await?;
			let clip = wav::Clip { samples, channels: 2, rate: SAMPLE_RATE };
			wav::write(out, &clip)?;
			let mono = pcm::to_mono(&clip.samples, 2);
			println!(
				"recorded {:.1} s to {} (energy {:.5})",
				mono.len() as f32 / SAMPLE_RATE as f32,
				out.display(),
				pcm::energy(&mono)
			);
			if let Some(freq) = expect_tone {
				let ratio = pcm::voiced_tone_ratio(&mono, *freq, SAMPLE_RATE);
				println!("{freq} Hz tone ratio: {ratio:.3}");
				if ratio < 0.5 {
					bail!("expected a {freq} Hz tone, ratio {ratio:.3} < 0.5");
				}
			}
			Ok(())
		}
	}
}

/// Read a WAV file and convert it to 48 kHz with the codec's channel count.
fn load(path: &std::path::Path, channels: usize) -> Result<Vec<f32>> {
	let clip = wav::read(path).with_context(|| format!("failed to read {}", path.display()))?;
	let samples = resample::resample_all(&clip.samples, clip.channels, clip.rate, SAMPLE_RATE)?;
	let mono = pcm::to_mono(&samples, clip.channels);
	Ok(pcm::from_mono(&mono, channels))
}

/// Send in real time: one frame every 20 ms, driving the connection in between.
async fn send(con: &mut Connection, codec: VoiceCodec, samples: &[f32]) -> Result<()> {
	let mut encoder = VoiceEncoder::new(codec)?;
	let mut frames = Vec::new();
	let mut framer = Framer::new(codec.frame_len());
	framer.push(samples, |f| frames.push(f.to_vec()));
	framer.flush(|f| frames.push(f.to_vec()));

	let mut next = Instant::now();
	for frame in &frames {
		con.send_audio(encoder.encode(frame)?)?;
		next += FRAME;
		if pump(con, Some(next), |_, _| Ok(Flow::Continue)).await? == PumpEnd::Interrupted {
			break;
		}
	}
	con.send_audio(encoder.end_of_stream())?;
	// Let the last packets go out.
	pump(con, Some(Instant::now() + Duration::from_millis(200)), |_, _| Ok(Flow::Continue)).await?;
	println!("sent {} frames ({:.1} s)", frames.len(), frames.len() as f32 * 0.02);
	Ok(())
}

/// Collect voice for `length`, mixing all speakers into stereo.
async fn record(con: &mut Connection, length: Duration) -> Result<Vec<f32>> {
	let mut mixer = Mixer::new();
	let mut out = Vec::new();
	let end = Instant::now() + length;
	let mut next = Instant::now() + FRAME;
	while next <= end {
		let end = pump(con, Some(next), |_, item| {
			if let StreamItem::Audio(packet) = item {
				let from = match packet.data().data() {
					AudioData::S2C { from, .. } | AudioData::S2CWhisper { from, .. } => *from,
					_ => return Ok(Flow::Continue),
				};
				// Late or duplicate packets are expected on UDP; the mixer drops them.
				let _ = mixer.handle_packet(ClientId(from), packet);
			}
			Ok(Flow::Continue)
		})
		.await?;
		let mut frame = vec![0.0; FRAME_SAMPLES * 2];
		mixer.fill_buffer(&mut frame);
		out.extend_from_slice(&frame);
		if end == PumpEnd::Interrupted {
			break;
		}
		next += FRAME;
	}
	Ok(out)
}
