//! Voelin patch: connection-bound myTeamSpeak proof and live account changes.

use tsproto::myts::Identity;
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};

use crate::{ConnectOptions, Connection, ConnectionState, Error, MessageHandle, Result};

impl ConnectOptions {
	/// Snapshot the account credential for the initial connection and reconnects.
	/// This is separate from the server identity and account profile UUID.
	pub fn myts_identity(mut self, identity: Option<Identity>) -> Self {
		self.myts_identity = identity;
		self
	}
}

impl Connection {
	/// Publish a connection-bound account proof, or clear the account identity.
	///
	/// Always updates the reconnect options, even if currently disconnected or
	/// sending fails. A running connection attempt owns its previous snapshot:
	/// callers must cancel and rebuild that attempt after `NotConnected`.
	/// Await the returned handle's `StreamItem::MessageResult` for server acceptance.
	pub fn update_myts_identity(&mut self, identity: Option<Identity>) -> Result<MessageHandle> {
		self.options.myts_identity = identity;
		let iv = match &self.state {
			ConnectionState::Connected { con, .. } => {
				con.client.params.as_ref().ok_or(Error::InitserverParamsMissing)?.shared_iv
			}
			_ => return Err(Error::NotConnected),
		};
		let packet = update_packet(self.options.myts_identity.as_ref(), &iv);
		self.send_command_with_result(packet)
	}
}

fn update_packet(identity: Option<&Identity>, iv: &[u8; 64]) -> OutCommand {
	let mut packet =
		OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "updatemytsid");
	if let Some(identity) = identity {
		identity.proof(iv).write_to(&mut packet);
	} else {
		// Empty arguments are serialized without '=' by the existing writer.
		packet.write_arg("myTeamspeakId", &"");
	}
	packet
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clear_contains_no_previous_account_proof() {
		let packet = update_packet(None, &[0; 64]).into_packet();
		assert_eq!(packet.content(), b"updatemytsid myTeamspeakId");
	}

	#[test]
	fn update_uses_current_connection_challenge() {
		// Scalar one and the standard compressed Edwards base point.
		let mut private = [0; 32];
		private[0] = 1;
		let mut public = [0x66; 32];
		public[0] = 0x58;
		let identity = Identity::new(vec![1; 33], 42, public, private, vec![2; 65]).unwrap();
		let first = update_packet(Some(&identity), &[0; 64]).into_packet();
		let second = update_packet(Some(&identity), &[1; 64]).into_packet();
		assert_ne!(first.content(), second.content());
		let wire = std::str::from_utf8(first.content()).unwrap();
		assert!(wire.starts_with("updatemytsid myTeamspeakId="));
		assert!(wire.contains(" acTime=42 "));
		assert!(!wire.contains("client_myteamspeak_id"));
		assert_eq!(wire.split(' ').count(), 7);
	}

	#[test]
	fn failed_update_still_replaces_reconnect_snapshot() {
		// Building opens no socket until the connection future is polled.
		let mut private = [0; 32];
		private[0] = 1;
		let mut public = [0x66; 32];
		public[0] = 0x58;
		let identity = Identity::new(vec![1; 33], 42, public, private, vec![2; 65]).unwrap();
		let mut connection =
			Connection::build("localhost").myts_identity(Some(identity)).connect().unwrap();
		assert!(connection.options.myts_identity.is_some());
		assert!(matches!(connection.update_myts_identity(None), Err(Error::NotConnected)));
		assert!(connection.options.myts_identity.is_none());
	}
}
