//! Host candidates behind mDNS names (`<uuid>.local`): browsers and other
//! libwebrtc builds hide their host addresses this way, and str0m takes
//! only IP addresses. [`resolve`] asks with a multicast DNS query
//! (RFC 6762) for the name's IPv4 address.
//!
//! The query goes out from port 5353, shared with the system's responder
//! (`SO_REUSEADDR`), and the answer is read from the multicast group:
//! Chromium's responder answers only such queries, not "legacy" ones from
//! another port (checked against Chromium 152). Where 5353 cannot be
//! shared, the query goes out from a port of our own, which responders
//! that follow RFC 6762 answer directly.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::Instant;

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;

/// The mDNS name of a candidate line (`candidate:...`, `a=` prefix
/// optional), if its address is one.
pub fn candidate_name(line: &str) -> Option<&str> {
	let line = line.trim();
	if !line.trim_start_matches("a=").starts_with("candidate:") {
		return None;
	}
	let address = line.split_whitespace().nth(4)?;
	address.to_ascii_lowercase().ends_with(".local").then_some(address)
}

/// The IPv4 address of `name` (`<uuid>.local`), asked on the local network
/// for up to `timeout`; `None` if nobody answered.
pub async fn resolve(name: &str, timeout: Duration) -> Option<Ipv4Addr> {
	let query = query(name)?;
	let socket = multicast_socket(PORT).or_else(|_| multicast_socket(0)).ok()?;
	let deadline = Instant::now() + timeout;
	let mut buf = vec![0; 1500];
	// Asked again after 0.5 s and 1.5 s: a first query may go unanswered.
	let mut again = [Duration::from_millis(500), Duration::from_secs(1)].into_iter();
	loop {
		socket.send_to(&query, (GROUP, PORT)).await.ok()?;
		let wait = again.next().map_or(deadline, |d| (Instant::now() + d).min(deadline));
		while let Ok(received) = tokio::time::timeout_at(wait, socket.recv_from(&mut buf)).await {
			let (n, _) = received.ok()?;
			if let Some(ip) = answer(&buf[..n], name) {
				return Some(ip);
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
	}
}

/// A UDP socket on `port` (shared) that receives the mDNS group.
fn multicast_socket(port: u16) -> std::io::Result<UdpSocket> {
	let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
	socket.set_reuse_address(true)?;
	socket.set_nonblocking(true)?;
	socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
	socket.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
	socket.set_multicast_ttl_v4(255)?;
	UdpSocket::from_std(socket.into())
}

/// A query for the A record of `name`; `None` for a name DNS cannot carry.
fn query(name: &str) -> Option<Vec<u8>> {
	// Id 0, no flags, one question.
	let mut packet = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
	for label in name.trim_end_matches('.').split('.') {
		let len = u8::try_from(label.len()).ok().filter(|l| (1..64).contains(l))?;
		packet.push(len);
		packet.extend_from_slice(label.as_bytes());
	}
	// Root, type A, class IN.
	packet.extend_from_slice(&[0, 0, 1, 0, 1]);
	Some(packet)
}

/// The address an mDNS response gives `name` (an A record in any
/// section), if it does.
fn answer(packet: &[u8], name: &str) -> Option<Ipv4Addr> {
	let u16_at = |at: usize| Some(u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]));
	// A response.
	if packet.get(2)? & 0x80 == 0 {
		return None;
	}
	let questions = u16_at(4)?;
	let records = u16_at(6)? as usize + u16_at(8)? as usize + u16_at(10)? as usize;
	let mut at = 12;
	for _ in 0..questions {
		at = read_name(packet, at)?.1 + 4;
	}
	for _ in 0..records {
		let (owner, end) = read_name(packet, at)?;
		let (kind, len) = (u16_at(end)?, usize::from(u16_at(end + 8)?));
		let data = packet.get(end + 10..end + 10 + len)?;
		if kind == 1 && len == 4 && owner.eq_ignore_ascii_case(name.trim_end_matches('.')) {
			return Some(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
		}
		at = end + 10 + len;
	}
	None
}

/// The (dotted) name at `at`, following compression pointers, and where
/// the record goes on after it.
fn read_name(packet: &[u8], mut at: usize) -> Option<(String, usize)> {
	let mut name = String::new();
	let mut end = None;
	// Pointers only go back, but a crafted packet could loop.
	for _ in 0..64 {
		let len = *packet.get(at)?;
		match len {
			0 => return Some((name, end.unwrap_or(at + 1))),
			_ if len & 0xc0 == 0xc0 => {
				end.get_or_insert(at + 2);
				at = usize::from(u16::from_be_bytes([len & 0x3f, *packet.get(at + 1)?]));
			}
			_ => {
				let label = packet.get(at + 1..at + 1 + usize::from(len))?;
				if !name.is_empty() {
					name.push('.');
				}
				name.push_str(&String::from_utf8_lossy(label));
				at += 1 + usize::from(len);
			}
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn names_in_candidates() {
		let line = "a=candidate:2387469048 1 udp 2113937151 6e2d1f23-173c-492f-bfef-16577595fc11.local \
		            39386 typ host generation 0";
		assert_eq!(candidate_name(line), Some("6e2d1f23-173c-492f-bfef-16577595fc11.local"));
		assert_eq!(candidate_name("candidate:1 1 udp 2130706431 10.0.0.2 50000 typ host"), None);
		assert_eq!(candidate_name("a=mid:0"), None);
	}

	#[test]
	fn query_and_answer() {
		let name = "abc-1.local";
		let q = query(name).unwrap();
		assert_eq!(&q[12..], b"\x05abc-1\x05local\x00\x00\x01\x00\x01");
		assert_eq!(query(&"x".repeat(64)), None);
		// A response as Chromium sends it: the question repeated, the A
		// record with the cache-flush class, its owner a pointer to the
		// question's name; another record first.
		let mut r = vec![0, 0, 0x84, 0, 0, 1, 0, 2, 0, 0, 0, 0];
		r.extend_from_slice(&q[12..]);
		r.extend_from_slice(b"\xc0\x0c\x00\x10\x80\x01\x00\x00\x00\x78\x00\x02\x01x");
		r.extend_from_slice(b"\xc0\x0c\x00\x01\x80\x01\x00\x00\x00\x78\x00\x04\xc0\xa8\x01\x07");
		assert_eq!(answer(&r, "ABC-1.local."), Some(Ipv4Addr::new(192, 168, 1, 7)));
		assert_eq!(answer(&r, "other.local"), None);
		// A query is not an answer; truncated or looping packets are none.
		assert_eq!(answer(&q, name), None);
		assert_eq!(answer(&r[..r.len() - 2], name), None);
		let mut looped = r.clone();
		looped[12] = 0xc0;
		looped[13] = 12;
		assert_eq!(answer(&looped, name), None);
	}
}
