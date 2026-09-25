//! Minimal STUN binding client (RFC 5389) to learn our public address.
//!
//! TeamSpeak streams use STUN only (`turn.teamspeak.com`), no TURN relay, so a
//! server-reflexive candidate is all we need beyond host candidates.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Default STUN servers of the TeamSpeak infrastructure.
pub const TEAMSPEAK_STUN: &[&str] = &["turn.teamspeak.com:3478", "turn2.teamspeak.com:3478"];

pub type TransactionId = [u8; 12];

/// A binding request without attributes.
pub fn binding_request(id: &TransactionId) -> Vec<u8> {
	let mut buf = Vec::with_capacity(20);
	buf.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
	buf.extend_from_slice(&0u16.to_be_bytes());
	buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
	buf.extend_from_slice(id);
	buf
}

/// The mapped address from a binding success response with transaction `id`.
pub fn parse_binding_response(data: &[u8], id: &TransactionId) -> Option<SocketAddr> {
	if data.len() < 20
		|| u16::from_be_bytes([data[0], data[1]]) != BINDING_SUCCESS
		|| u32::from_be_bytes(data[4..8].try_into().ok()?) != MAGIC_COOKIE
		|| &data[8..20] != id
	{
		return None;
	}
	let len = usize::from(u16::from_be_bytes([data[2], data[3]]));
	let attrs = data.get(20..20 + len)?;
	let mut pos = 0;
	let mut mapped = None;
	while pos + 4 <= attrs.len() {
		let kind = u16::from_be_bytes([attrs[pos], attrs[pos + 1]]);
		let alen = usize::from(u16::from_be_bytes([attrs[pos + 2], attrs[pos + 3]]));
		let value = attrs.get(pos + 4..pos + 4 + alen)?;
		match kind {
			ATTR_XOR_MAPPED_ADDRESS => return parse_address(value, Some(&data[4..20])),
			ATTR_MAPPED_ADDRESS => mapped = parse_address(value, None),
			_ => {}
		}
		pos += 4 + alen.div_ceil(4) * 4;
	}
	mapped
}

/// Parse a (XOR-)MAPPED-ADDRESS value; `xor` is cookie + transaction id.
fn parse_address(value: &[u8], xor: Option<&[u8]>) -> Option<SocketAddr> {
	if value.len() < 4 {
		return None;
	}
	let family = value[1];
	let mut port = u16::from_be_bytes([value[2], value[3]]);
	let mut addr = value[4..].to_vec();
	if let Some(x) = xor {
		port ^= (MAGIC_COOKIE >> 16) as u16;
		for (b, k) in addr.iter_mut().zip(x) {
			*b ^= k;
		}
	}
	let ip = match family {
		1 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(addr.get(..4)?).ok()?)),
		2 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(addr.get(..16)?).ok()?)),
		_ => return None,
	};
	Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn xor_mapped_address() {
		let id = [7u8; 12];
		// Response for 203.0.113.5:40000.
		let mut resp = Vec::new();
		resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
		resp.extend_from_slice(&12u16.to_be_bytes());
		resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
		resp.extend_from_slice(&id);
		resp.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
		resp.extend_from_slice(&8u16.to_be_bytes());
		resp.extend_from_slice(&[0, 1]);
		resp.extend_from_slice(&(40000u16 ^ 0x2112).to_be_bytes());
		let ip = u32::from(Ipv4Addr::new(203, 0, 113, 5)) ^ MAGIC_COOKIE;
		resp.extend_from_slice(&ip.to_be_bytes());
		assert_eq!(parse_binding_response(&resp, &id), Some("203.0.113.5:40000".parse().unwrap()));
		assert_eq!(parse_binding_response(&resp, &[8u8; 12]), None);
		assert_eq!(binding_request(&id).len(), 20);
	}
}
