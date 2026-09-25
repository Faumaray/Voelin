//! Raw UDP packets from the server.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tsproto_packets::packets::{Direction, InPacket};

fuzz_target!(|data: &[u8]| {
	for dir in [Direction::S2C, Direction::C2S] {
		if let Ok(packet) = InPacket::try_new(dir, data) {
			let _ = format!("{packet:?}");
			let _ = packet.header().packet_type();
			let _ = packet.ack_packet();
		}
	}
});
