//! JSON messages carried in `streamsignaling` / `notifystreamsignaling`.
//!
//! The server passes the `json` parameter through unchanged. The format is
//! `{"cmd": <name>, "args": {...}}`; different clients spell the argument keys
//! differently, so parsing accepts every known variant and serializing uses the
//! spelling the official client sends.

use serde_json::{Map, Value, json};

/// One signalling message between streamer and viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
	/// SDP offer. `reconnect` is set for `reconnectOffer` (ICE restart).
	Offer { sdp: String, reconnect: bool },
	/// SDP answer to an offer.
	Answer { sdp: String },
	/// Trickled ICE candidate (`candidate:...` line, with or without the `a=` prefix).
	IceCandidate { candidate: String, mid: Option<String>, mline_index: Option<u32> },
	/// The other side asks for a new offer.
	Reconnect,
	/// A command this client does not know; kept for logging.
	Unknown { cmd: String, args: Value },
}

/// The `json` parameter could not be understood.
#[derive(Debug, thiserror::Error)]
pub enum SignalError {
	#[error("invalid JSON: {0}")]
	Json(#[from] serde_json::Error),
	#[error("missing \"cmd\"")]
	MissingCmd,
	#[error("{cmd}: missing argument {arg}")]
	MissingArg { cmd: String, arg: &'static str },
}

fn string_arg(args: &Map<String, Value>, keys: &[&str]) -> Option<String> {
	keys.iter().find_map(|k| args.get(*k)).and_then(|v| v.as_str()).map(str::to_owned)
}

impl Signal {
	pub fn parse(text: &str) -> Result<Self, SignalError> {
		let value: Value = serde_json::from_str(text)?;
		let cmd = value.get("cmd").and_then(Value::as_str).ok_or(SignalError::MissingCmd)?;
		let empty = Map::new();
		let args = value.get("args").and_then(Value::as_object).unwrap_or(&empty);
		let missing = |arg| SignalError::MissingArg { cmd: cmd.to_owned(), arg };
		Ok(match cmd {
			"offer" | "reconnectOffer" => Signal::Offer {
				sdp: string_arg(args, &["offer", "sdp"]).ok_or_else(|| missing("offer"))?,
				reconnect: cmd == "reconnectOffer",
			},
			"answer" => Signal::Answer {
				sdp: string_arg(args, &["answer", "sdp"]).ok_or_else(|| missing("answer"))?,
			},
			"iceCandidate" | "candidate" => Signal::IceCandidate {
				candidate: string_arg(args, &["candidate", "sdp"])
					.ok_or_else(|| missing("candidate"))?,
				mid: string_arg(args, &["mid", "sdpMid", "sdp_mid"]),
				mline_index: ["mLine", "sdpMLineIndex", "sdpMlineIndex", "sdp_mline_index"]
					.iter()
					.find_map(|k| args.get(*k))
					.and_then(Value::as_u64)
					.and_then(|v| u32::try_from(v).ok()),
			},
			"reconnect" => Signal::Reconnect,
			other => Signal::Unknown { cmd: other.to_owned(), args: Value::Object(args.clone()) },
		})
	}

	pub fn to_json(&self) -> String {
		let value = match self {
			Signal::Offer { sdp, reconnect } => json!({
				"cmd": if *reconnect { "reconnectOffer" } else { "offer" },
				"args": { "offer": sdp },
			}),
			Signal::Answer { sdp } => json!({ "cmd": "answer", "args": { "answer": sdp } }),
			Signal::IceCandidate { candidate, mid, mline_index } => json!({
				"cmd": "iceCandidate",
				"args": { "candidate": candidate, "sdpMid": mid, "sdpMLineIndex": mline_index },
			}),
			Signal::Reconnect => json!({ "cmd": "reconnect", "args": {} }),
			Signal::Unknown { cmd, args } => json!({ "cmd": cmd, "args": args }),
		};
		value.to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn roundtrip() {
		let signals = [
			Signal::Offer { sdp: "v=0\r\n".into(), reconnect: false },
			Signal::Offer { sdp: "v=0\r\n".into(), reconnect: true },
			Signal::Answer { sdp: "v=0\r\n".into() },
			Signal::IceCandidate {
				candidate: "candidate:1 1 udp 2130706431 10.0.0.2 50000 typ host".into(),
				mid: Some("0".into()),
				mline_index: Some(0),
			},
			Signal::Reconnect,
		];
		for s in signals {
			assert_eq!(Signal::parse(&s.to_json()).unwrap(), s);
		}
	}

	#[test]
	fn key_variants() {
		let s = Signal::parse(r#"{"cmd":"answer","args":{"sdp":"x"}}"#).unwrap();
		assert_eq!(s, Signal::Answer { sdp: "x".into() });
		let s = Signal::parse(r#"{"cmd":"iceCandidate","args":{"sdp":"c","mid":"1","mLine":1}}"#)
			.unwrap();
		assert_eq!(
			s,
			Signal::IceCandidate {
				candidate: "c".into(),
				mid: Some("1".into()),
				mline_index: Some(1)
			}
		);
		let s = Signal::parse(r#"{"cmd":"reconnectOffer","args":{"offer":"o"}}"#).unwrap();
		assert_eq!(s, Signal::Offer { sdp: "o".into(), reconnect: true });
	}

	#[test]
	fn errors() {
		assert!(matches!(Signal::parse("nope"), Err(SignalError::Json(_))));
		assert!(matches!(Signal::parse("{}"), Err(SignalError::MissingCmd)));
		assert!(matches!(
			Signal::parse(r#"{"cmd":"answer","args":{}}"#),
			Err(SignalError::MissingArg { arg: "answer", .. })
		));
		assert!(matches!(Signal::parse(r#"{"cmd":"future"}"#), Ok(Signal::Unknown { .. })));
	}
}
