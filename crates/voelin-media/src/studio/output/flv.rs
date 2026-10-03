//! FLV tags: what libavformat's RTMP protocol takes, one RTMP message per
//! tag. Its writer reads a 13-byte file header first, then per tag the
//! 11-byte tag header, the body and the 4-byte size after it, so what goes
//! into an RTMP connection is exactly an FLV file.
//!
//! Only what a live stream needs: the script tag with `onMetaData` (AMF0),
//! H.264 as AVC tags (a sequence header with the `avcC` record, then NAL
//! units with 4-byte lengths, no B-frames so composition time 0) and AAC (a
//! sequence header with the `AudioSpecificConfig`, then raw frames).

use crate::studio::output::ebml;

/// The file header (audio and video), and the size of the tag before the
/// first one (0).
pub const HEADER: [u8; 13] = [b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0];

/// Tag types.
pub const AUDIO: u8 = 8;
pub const VIDEO: u8 = 9;
pub const SCRIPT: u8 = 18;

/// The audio tag flags of AAC: format 10, "44 kHz" (what FLV says for any
/// AAC rate; the sequence header has the real one), 16 bit, stereo.
const AAC_FLAGS: u8 = 0xAF;
/// The video tag codec id of H.264 (AVC).
const AVC: u8 = 7;

/// Append one tag of `kind` at `ms` (milliseconds; FLV keeps 32 bits) whose
/// body `body` appends.
pub fn tag(out: &mut Vec<u8>, kind: u8, ms: u32, body: impl FnOnce(&mut Vec<u8>)) {
	let start = out.len();
	out.push(kind);
	out.extend_from_slice(&[0; 3]); // body size, below
	out.extend_from_slice(&ms.to_be_bytes()[1..]);
	out.push((ms >> 24) as u8);
	out.extend_from_slice(&[0; 3]); // stream id
	let body_start = out.len();
	body(out);
	let size = (out.len() - body_start) as u32;
	out[start + 1..start + 4].copy_from_slice(&size.to_be_bytes()[1..]);
	out.extend_from_slice(&(size + 11).to_be_bytes());
}

/// What `onMetaData` says about the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
	pub width: u32,
	pub height: u32,
	/// Audio sample rate (Hz) and channels.
	pub sample_rate: u32,
	pub channels: u16,
}

/// The body of the script tag: `onMetaData` and an ECMA array.
pub fn metadata(out: &mut Vec<u8>, meta: &Metadata) {
	fn string(out: &mut Vec<u8>, text: &str) {
		out.extend_from_slice(&(text.len() as u16).to_be_bytes());
		out.extend_from_slice(text.as_bytes());
	}
	enum Value<'a> {
		Number(f64),
		Bool(bool),
		Text(&'a str),
	}
	let entries = [
		("duration", Value::Number(0.0)),
		("width", Value::Number(f64::from(meta.width))),
		("height", Value::Number(f64::from(meta.height))),
		("videocodecid", Value::Number(f64::from(AVC))),
		("audiocodecid", Value::Number(10.0)),
		("audiosamplerate", Value::Number(f64::from(meta.sample_rate))),
		("audiosamplesize", Value::Number(16.0)),
		("stereo", Value::Bool(meta.channels > 1)),
		("encoder", Value::Text("Voelin")),
	];
	out.push(2);
	string(out, "onMetaData");
	out.push(8);
	out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
	for (key, value) in entries {
		string(out, key);
		match value {
			Value::Number(n) => {
				out.push(0);
				out.extend_from_slice(&n.to_be_bytes());
			}
			Value::Bool(b) => out.extend_from_slice(&[1, u8::from(b)]),
			Value::Text(t) => {
				out.push(2);
				string(out, t);
			}
		}
	}
	out.extend_from_slice(&[0, 0, 9]);
}

/// The body of the AVC sequence header: the `avcC` record
/// ([`ebml::avcc`]).
pub fn avc_sequence_header(out: &mut Vec<u8>, avcc: &[u8]) {
	out.extend_from_slice(&[0x10 | AVC, 0, 0, 0, 0]);
	out.extend_from_slice(avcc);
}

/// The body of one H.264 frame given in Annex B: its NAL units with 4-byte
/// lengths (access unit delimiters left out, as in a recording).
pub fn avc_frame(out: &mut Vec<u8>, keyframe: bool, annex_b: &[u8]) {
	let frame_type = if keyframe { 0x10 } else { 0x20 };
	out.extend_from_slice(&[frame_type | AVC, 1, 0, 0, 0]);
	for unit in ebml::annex_b_units(annex_b) {
		if unit[0] & 0x1F == 9 {
			continue;
		}
		out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
		out.extend_from_slice(unit);
	}
}

/// The body of the AAC sequence header.
pub fn aac_sequence_header(out: &mut Vec<u8>, audio_specific_config: &[u8]) {
	out.extend_from_slice(&[AAC_FLAGS, 0]);
	out.extend_from_slice(audio_specific_config);
}

/// The body of one raw AAC frame.
pub fn aac_frame(out: &mut Vec<u8>, data: &[u8]) {
	out.extend_from_slice(&[AAC_FLAGS, 1]);
	out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The tags of an FLV stream: (type, time, body).
	fn tags(mut data: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
		let mut tags = Vec::new();
		while data.len() >= 11 {
			let size = u32::from_be_bytes([0, data[1], data[2], data[3]]) as usize;
			let ms = u32::from_be_bytes([data[7], data[4], data[5], data[6]]);
			tags.push((data[0], ms, data[11..11 + size].to_vec()));
			let previous = u32::from_be_bytes(data[11 + size..15 + size].try_into().unwrap());
			assert_eq!(previous as usize, size + 11);
			data = &data[15 + size..];
		}
		assert!(data.is_empty());
		tags
	}

	#[test]
	fn tags_have_sizes_and_times() {
		let mut out = Vec::new();
		tag(&mut out, AUDIO, 21, |b| aac_frame(b, &[1, 2, 3]));
		// Times past 24 bits use the extension byte.
		tag(&mut out, VIDEO, 0x0123_4567, |b| avc_frame(b, false, &[0, 0, 0, 1, 0x41, 9, 9]));
		let tags = tags(&out);
		assert_eq!(tags[0], (AUDIO, 21, vec![0xAF, 1, 1, 2, 3]));
		assert_eq!(tags[1], (VIDEO, 0x0123_4567, vec![0x27, 1, 0, 0, 0, 0, 0, 0, 3, 0x41, 9, 9]));
	}

	#[test]
	fn h264_becomes_length_prefixed() {
		let keyframe = [
			0, 0, 0, 1, 0x09, 0xF0, // access unit delimiter: dropped
			0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1F, 0xAC, // SPS
			0, 0, 1, 0x68, 0xEE, 0x3C, // PPS
			0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
		];
		let mut body = Vec::new();
		avc_frame(&mut body, true, &keyframe);
		assert_eq!(
			body,
			[
				0x17, 1, 0, 0, 0, 0, 0, 0, 5, 0x67, 0x64, 0x00, 0x1F, 0xAC, 0, 0, 0, 3, 0x68, 0xEE,
				0x3C, 0, 0, 0, 3, 0x65, 0x88, 0x84
			]
		);
		let avcc = ebml::avcc(&keyframe).unwrap();
		let mut header = Vec::new();
		avc_sequence_header(&mut header, &avcc);
		assert_eq!(&header[..5], [0x17, 0, 0, 0, 0]);
		// avcC: version 1, profile 100, compatibility 0, level 31.
		assert_eq!(&header[5..9], [1, 0x64, 0x00, 0x1F]);
	}

	#[test]
	fn metadata_is_amf0() {
		let mut body = Vec::new();
		metadata(
			&mut body,
			&Metadata { width: 1280, height: 720, sample_rate: 48_000, channels: 2 },
		);
		assert_eq!(&body[..13], b"\x02\x00\x0aonMetaData");
		assert_eq!(body[13], 8);
		assert_eq!(u32::from_be_bytes(body[14..18].try_into().unwrap()), 9);
		assert!(body.ends_with(&[0, 0, 9]));
		let width = body.windows(5).position(|w| w == b"width").unwrap() + 5;
		assert_eq!(body[width], 0);
		assert_eq!(f64::from_be_bytes(body[width + 1..width + 9].try_into().unwrap()), 1280.0);
		let stereo = body.windows(6).position(|w| w == b"stereo").unwrap() + 6;
		assert_eq!(&body[stereo..stereo + 2], [1, 1]);
		assert_eq!(HEADER.len(), 13);
	}
}
