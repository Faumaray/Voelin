//! The license chain sent in `initivexpand2 l=...` during the handshake.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tsproto::license::Licenses;

fuzz_target!(|data: &[u8]| {
	if let Ok(licenses) = Licenses::parse_ignore_expired(data.to_vec()) {
		let _ = format!("{licenses:?}");
	}
});
