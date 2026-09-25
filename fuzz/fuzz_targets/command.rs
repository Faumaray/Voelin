//! Decrypted command payloads, parsed into typed messages.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ts_bookkeeping::messages::s2c::InMessage;
use tsproto_packets::commands::CommandParser;
use tsproto_packets::packets::{Direction, InPacket};

/// S2C header: 8 byte MAC, packet id 1, type Command with the NewProtocol flag.
const HEADER: [u8; 11] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x22];

fuzz_target!(|data: &[u8]| {
	let (_, args) = CommandParser::new(data);
	for arg in args {
		let _ = format!("{arg:?}");
	}

	let mut packet = HEADER.to_vec();
	packet.extend_from_slice(data);
	let packet = InPacket::new(Direction::S2C, &packet);
	if let Ok(msg) = InMessage::new(&packet.header(), data) {
		let _ = format!("{msg:?}");
	}
});
