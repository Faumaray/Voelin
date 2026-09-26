//! Opus encoding into TeamSpeak voice packets.

use opus2::{Application, Bitrate, Channels, Encoder};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

use crate::pcm::{FRAME_SAMPLES, SAMPLE_RATE};
use crate::{Error, Result};

/// Largest possible Opus packet.
const MAX_PACKET: usize = 1275;

/// The two Opus codecs TeamSpeak channels use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceCodec {
	/// Mono, tuned for speech (`CodecType::OpusVoice`).
	Voice,
	/// Stereo, tuned for music (`CodecType::OpusMusic`).
	Music,
}

impl VoiceCodec {
	pub fn channels(self) -> usize {
		match self {
			VoiceCodec::Voice => 1,
			VoiceCodec::Music => 2,
		}
	}

	pub fn codec_type(self) -> CodecType {
		match self {
			VoiceCodec::Voice => CodecType::OpusVoice,
			VoiceCodec::Music => CodecType::OpusMusic,
		}
	}

	/// Samples (all channels) in one 20 ms frame.
	pub fn frame_len(self) -> usize {
		FRAME_SAMPLES * self.channels()
	}
}

/// Encodes 20 ms frames of 48 kHz audio into voice packets.
pub struct VoiceEncoder {
	encoder: Encoder,
	codec: VoiceCodec,
	out: Vec<u8>,
}

impl VoiceEncoder {
	pub fn new(codec: VoiceCodec) -> Result<Self> {
		let (channels, application) = match codec {
			VoiceCodec::Voice => (Channels::Mono, Application::Voip),
			VoiceCodec::Music => (Channels::Stereo, Application::Audio),
		};
		let mut encoder = Encoder::new(SAMPLE_RATE, channels, application)?;
		// Survive some packet loss: TeamSpeak voice is plain UDP.
		encoder.set_inband_fec(true)?;
		encoder.set_packet_loss_perc(5)?;
		Ok(Self { encoder, codec, out: vec![0; MAX_PACKET] })
	}

	pub fn codec(&self) -> VoiceCodec {
		self.codec
	}

	/// Target bitrate in bits per second.
	pub fn set_bitrate(&mut self, bps: i32) -> Result<()> {
		self.encoder.set_bitrate(Bitrate::Bits(bps))?;
		Ok(())
	}

	/// Encode one frame of [`VoiceCodec::frame_len`] interleaved samples.
	pub fn encode_to_bytes(&mut self, frame: &[f32]) -> Result<&[u8]> {
		if frame.len() != self.codec.frame_len() {
			return Err(Error::Invalid(format!(
				"frame has {} samples, expected {}",
				frame.len(),
				self.codec.frame_len()
			)));
		}
		let len = self.encoder.encode_float(frame, &mut self.out)?;
		Ok(&self.out[..len])
	}

	/// Encode one frame into a packet ready for `Connection::send_audio`.
	/// The connection assigns the packet id.
	pub fn encode(&mut self, frame: &[f32]) -> Result<OutPacket> {
		let codec = self.codec.codec_type();
		let data = self.encode_to_bytes(frame)?;
		Ok(OutAudio::new(&AudioData::C2S { id: 0, codec, data }))
	}

	/// The empty packet that tells listeners the transmission ended.
	pub fn end_of_stream(&self) -> OutPacket {
		OutAudio::new(&AudioData::C2S { id: 0, codec: self.codec.codec_type(), data: &[] })
	}
}

#[cfg(test)]
mod tests {
	use tsproto_packets::packets::{Direction, InAudioBuf};

	use super::*;
	use crate::pcm;

	/// Encode a tone, feed the packets through the mixer (jitter buffer +
	/// decoder), and check the tone comes out.
	#[test]
	fn roundtrip_through_mixer() {
		let mut encoder = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let mut mixer = crate::Mixer::new();
		let tone = pcm::sine(1000.0, 1.0, 0.5);
		let mut decoded = Vec::new();
		for (i, frame) in tone.chunks_exact(FRAME_SAMPLES).enumerate() {
			let data = encoder.encode_to_bytes(frame).unwrap().to_vec();
			let packet = OutAudio::new(&AudioData::S2C {
				id: i as u16,
				codec: CodecType::OpusVoice,
				from: 7,
				data: &data,
			});
			let packet = InAudioBuf::try_new(Direction::S2C, packet.into_vec()).unwrap();
			mixer.handle_packet(tsclientlib::ClientId(7), packet).unwrap();
			let mut out = vec![0.0; FRAME_SAMPLES * 2];
			mixer.fill_buffer(&mut out);
			decoded.extend(pcm::to_mono(&out, 2));
		}
		// Skip the jitter buffer's initial latency and the codec warm-up.
		let tail = &decoded[decoded.len() / 2..];
		let ratio = pcm::tone_ratio(tail, 1000.0, SAMPLE_RATE);
		assert!(ratio > 0.8, "tone ratio {ratio}");
	}

	#[test]
	fn rejects_wrong_frame_size() {
		let mut encoder = VoiceEncoder::new(VoiceCodec::Music).unwrap();
		assert!(encoder.encode(&[0.0; FRAME_SAMPLES]).is_err());
		assert!(encoder.encode(&[0.0; FRAME_SAMPLES * 2]).is_ok());
	}
}
