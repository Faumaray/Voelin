//! Messages in both directions.

use serde::{Deserialize, Serialize};
use tsc_model::{ChatMessage, ChatTarget, PresenceDelta, PresenceSnapshot};

/// Every frame: `{"v":1,"id":7,"type":"send_chat","data":{...}}`.
///
/// `id` is chosen by the client for requests; the gateway copies it into the
/// answer ([`ServerMsg::Ok`], [`ServerMsg::Error`], [`ServerMsg::History`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<M> {
	pub v: u32,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<u64>,
	#[serde(flatten)]
	pub msg: M,
}

impl<M> Envelope<M> {
	pub fn new(msg: M) -> Self {
		Self { v: crate::VERSION, id: None, msg }
	}
	pub fn with_id(id: u64, msg: M) -> Self {
		Self { v: crate::VERSION, id: Some(id), msg }
	}
}

/// Client to gateway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientMsg {
	/// Answer to [`ServerMsg::Hello`].
	Auth {
		/// Public key, TeamSpeak "omega" format (base64 libtomcrypt DER).
		omega: String,
		/// Hash-cash counter (`client_key_offset`) that gives the identity its level.
		key_offset: u64,
		/// Unix time in seconds, part of the signed challenge.
		ts: i64,
		/// Base64 DER ECDSA signature over [`crate::challenge`].
		signature: String,
		/// Nickname for relayed messages.
		nickname: String,
	},
	/// Log in again with a token from [`ServerMsg::AuthOk`] instead of a signature.
	Resume {
		token: String,
		nickname: String,
	},
	/// Start (or restart) the presence stream: a snapshot, then deltas.
	SubscribePresence,
	UnsubscribePresence,
	/// Start receiving a chat.
	OpenChat {
		target: ChatTarget,
	},
	CloseChat {
		target: ChatTarget,
	},
	SendChat {
		target: ChatTarget,
		text: String,
	},
	/// Up to `limit` messages before message id `before` (latest when absent).
	History {
		target: ChatTarget,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		before: Option<i64>,
		limit: u32,
	},
	Ping,
}

/// Gateway to client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMsg {
	/// First message after connecting.
	Hello {
		gateway_id: String,
		server_uid: String,
		server_name: String,
		nonce: String,
		/// Optional features, e.g. `history`.
		#[serde(default)]
		capabilities: Vec<String>,
	},
	AuthOk {
		uid: String,
		/// Opaque; send it in [`ClientMsg::Resume`].
		token: String,
		/// Unix time in seconds.
		token_expires: i64,
	},
	/// Presence restricted to what this user may see. `seq` restarts at 0 with
	/// each snapshot and increases by one per delta.
	PresenceSnapshot {
		seq: u64,
		snapshot: PresenceSnapshot,
	},
	PresenceDelta {
		seq: u64,
		delta: PresenceDelta,
	},
	/// A message in an open chat. `id` orders messages and pages history.
	ChatEvent {
		id: i64,
		message: ChatMessage,
	},
	History {
		messages: Vec<HistoryEntry>,
	},
	/// Request succeeded.
	Ok,
	Error {
		code: ErrorCode,
		message: String,
	},
	Pong,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
	pub id: i64,
	pub message: ChatMessage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
	/// Malformed or unexpected message.
	BadRequest,
	/// Signature, timestamp or token invalid.
	AuthFailed,
	/// The server's database has no client with this unique id: connect
	/// with voice once first.
	UnknownIdentity,
	/// Identity security level below what the server requires.
	LevelTooLow,
	/// The user is banned on the server.
	Banned,
	/// Not allowed by the user's server permissions or the gateway config.
	Forbidden,
	NotAuthenticated,
	/// Temporary problem talking to the TeamSpeak server.
	Unavailable,
	Internal,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn envelope_json_shape() {
		let env = Envelope::with_id(
			7,
			ClientMsg::SendChat { target: ChatTarget::Channel(5), text: "hi".into() },
		);
		let json = serde_json::to_string(&env).unwrap();
		assert_eq!(
			json,
			r#"{"v":1,"id":7,"type":"send_chat","data":{"target":{"kind":"channel","id":5},"text":"hi"}}"#
		);
		let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
		assert_eq!(back, env);

		let ping = serde_json::to_string(&Envelope::new(ClientMsg::Ping)).unwrap();
		assert_eq!(ping, r#"{"v":1,"type":"ping"}"#);
		assert_eq!(
			serde_json::to_string(&Envelope::new(ServerMsg::Ok)).unwrap(),
			r#"{"v":1,"type":"ok"}"#
		);
	}

	#[test]
	fn ignores_unknown_fields() {
		let json = r#"{"v":1,"type":"history","data":{"target":{"kind":"server"},"limit":5,"future":true},"extra":1}"#;
		let env: Envelope<ClientMsg> = serde_json::from_str(json).unwrap();
		assert_eq!(
			env.msg,
			ClientMsg::History { target: ChatTarget::Server, before: None, limit: 5 }
		);
	}
}
