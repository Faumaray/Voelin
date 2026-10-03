//! A UDP relay between a streamer's and a viewer's peer on loopback that
//! loses packets on purpose: every n-th media packet towards the viewer
//! (losses that NACK and retransmissions must repair), or everything but
//! STUN and DTLS once the handshake picked certain SRTP profiles (a peer
//! whose SRTP fails after a successful handshake).
//!
//! Put it between the peers by rewriting their host candidates:
//! [`Relay::offer`] on the streamer's offer, [`Relay::answer`] on the
//! viewer's answer. Packets are told apart as RFC 7983 does.

#![allow(dead_code, reason = "each test file uses part of it")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::net::UdpSocket;

/// What the relay lets through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
	Pass,
	/// Every n-th RTP packet towards the viewer is lost (retransmissions
	/// included).
	LoseEvery(u64),
	/// Once a DTLS 1.2 ServerHello selects one of these SRTP profiles
	/// (RFC 5764 ids: 1 AES_CM_128_HMAC_SHA1_80, 7 AEAD_AES_128_GCM, 8
	/// AEAD_AES_256_GCM), no RTP or RTCP gets through either way.
	FailSrtp(&'static [u16]),
}

/// One end of the relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Towards {
	Viewer,
	Streamer,
}

struct State {
	rule: Mutex<Rule>,
	streamer: OnceLock<SocketAddr>,
	viewer: OnceLock<SocketAddr>,
	/// A ServerHello picked a failing profile.
	failing: AtomicBool,
	/// The SRTP profile the handshake selected (0 before).
	profile: AtomicU64,
	/// RTP packets towards the viewer, and how many were lost on purpose.
	media: AtomicU64,
	lost: AtomicU64,
}

pub struct Relay {
	/// Where the streamer reaches the viewer.
	a: SocketAddr,
	/// Where the viewer reaches the streamer.
	b: SocketAddr,
	state: Arc<State>,
	tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Relay {
	fn drop(&mut self) {
		for task in &self.tasks {
			task.abort();
		}
	}
}

/// The first host candidate's address in `sdp`.
pub fn host_candidate(sdp: &str) -> Option<SocketAddr> {
	sdp.lines().find_map(|l| {
		let parts: Vec<&str> = l.trim().split(' ').collect();
		let typ = parts.iter().position(|p| *p == "typ")?;
		if !parts[0].contains("candidate:") || parts.get(typ + 1) != Some(&"host") || typ < 2 {
			return None;
		}
		format!("{}:{}", parts[typ - 2], parts[typ - 1]).parse().ok()
	})
}

/// `sdp` with the candidate address `from` replaced by `to`.
fn replace(sdp: &str, from: SocketAddr, to: SocketAddr) -> String {
	sdp.replace(
		&format!(" {} {} typ ", from.ip(), from.port()),
		&format!(" {} {} typ ", to.ip(), to.port()),
	)
}

/// The SRTP profile a DTLS 1.2 ServerHello in `data` selects.
fn server_hello_profile(data: &[u8]) -> Option<u16> {
	// Handshake record; its first message a ServerHello.
	if data.first() != Some(&22) || data.get(13) != Some(&2) {
		return None;
	}
	// use_srtp: type 14, length 5, one profile, no MKI.
	data.windows(9).find_map(|w| {
		(w[..6] == [0, 14, 0, 5, 0, 2] && w[8] == 0).then(|| u16::from_be_bytes([w[6], w[7]]))
	})
}

/// RTP or RTCP (RFC 7983), and RTCP among them (RFC 5761).
fn is_media(data: &[u8]) -> bool {
	data.first().is_some_and(|b| (128..=191).contains(b))
}

fn is_rtcp(data: &[u8]) -> bool {
	is_media(data) && data.get(1).is_some_and(|b| (192..=223).contains(b))
}

impl State {
	/// Whether `data`, on its way `to`, gets through.
	fn passes(&self, to: Towards, data: &[u8]) -> bool {
		let rule = *self.rule.lock().unwrap_or_else(PoisonError::into_inner);
		if let Some(profile) = server_hello_profile(data) {
			self.profile.store(u64::from(profile), Ordering::Relaxed);
			let fails = matches!(rule, Rule::FailSrtp(set) if set.contains(&profile));
			self.failing.store(fails, Ordering::Relaxed);
		}
		if !is_media(data) {
			return true;
		}
		if self.failing.load(Ordering::Relaxed) {
			return false;
		}
		if to == Towards::Viewer && !is_rtcp(data) {
			let n = self.media.fetch_add(1, Ordering::Relaxed) + 1;
			if let Rule::LoseEvery(every) = rule
				&& n.is_multiple_of(every)
			{
				self.lost.fetch_add(1, Ordering::Relaxed);
				return false;
			}
		}
		true
	}
}

impl Relay {
	pub async fn new(rule: Rule) -> Self {
		let a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
		let b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
		let state = Arc::new(State {
			rule: Mutex::new(rule),
			streamer: OnceLock::new(),
			viewer: OnceLock::new(),
			failing: AtomicBool::new(false),
			profile: AtomicU64::new(0),
			media: AtomicU64::new(0),
			lost: AtomicU64::new(0),
		});
		let (addr_a, addr_b) = (a.local_addr().unwrap(), b.local_addr().unwrap());
		// From the streamer on a, to the viewer from b; and back.
		let forward = |from: Arc<UdpSocket>, to: Arc<UdpSocket>, towards: Towards| {
			let state = state.clone();
			tokio::spawn(async move {
				let mut buf = vec![0; 2048];
				loop {
					let Ok((n, _)) = from.recv_from(&mut buf).await else { continue };
					let target = match towards {
						Towards::Viewer => state.viewer.get(),
						Towards::Streamer => state.streamer.get(),
					};
					if let Some(target) = target
						&& state.passes(towards, &buf[..n])
					{
						let _ = to.send_to(&buf[..n], target).await;
					}
				}
			})
		};
		let tasks =
			vec![forward(a.clone(), b.clone(), Towards::Viewer), forward(b, a, Towards::Streamer)];
		Self { a: addr_a, b: addr_b, state, tasks }
	}

	/// The streamer's offer as the viewer gets it: the relay in place of
	/// the streamer.
	pub fn offer(&self, sdp: &str) -> String {
		let streamer = host_candidate(sdp).expect("a host candidate in the offer");
		let _ = self.state.streamer.set(streamer);
		replace(sdp, streamer, self.b)
	}

	/// The viewer's answer as the streamer gets it: the relay in place of
	/// the viewer.
	pub fn answer(&self, sdp: &str) -> String {
		let viewer = host_candidate(sdp).expect("a host candidate in the answer");
		let _ = self.state.viewer.set(viewer);
		replace(sdp, viewer, self.a)
	}

	pub fn set_rule(&self, rule: Rule) {
		*self.state.rule.lock().unwrap_or_else(PoisonError::into_inner) = rule;
	}

	/// The SRTP profile id the handshake selected, if it was seen.
	pub fn profile(&self) -> Option<u16> {
		let profile = self.state.profile.load(Ordering::Relaxed);
		(profile != 0).then_some(profile as u16)
	}

	/// RTP packets towards the viewer, and how many of them were lost.
	pub fn media(&self) -> (u64, u64) {
		(self.state.media.load(Ordering::Relaxed), self.state.lost.load(Ordering::Relaxed))
	}
}

#[test]
fn packets_are_told_apart() {
	let mut hello = vec![22, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 80, 2];
	hello.extend([0; 40]);
	hello.extend([0, 14, 0, 5, 0, 2, 0, 7, 0]);
	assert_eq!(server_hello_profile(&hello), Some(7));
	hello[13] = 1;
	assert_eq!(server_hello_profile(&hello), None, "a ClientHello");
	assert!(is_media(&[0x80, 96]) && !is_rtcp(&[0x80, 96]));
	assert!(is_rtcp(&[0x81, 201]) && !is_media(&[0, 1]));
	let sdp = "a=candidate:1 1 udp 2130706431 127.0.0.1 50000 typ host\r\n";
	let addr = host_candidate(sdp).unwrap();
	assert_eq!(addr, "127.0.0.1:50000".parse().unwrap());
	let to = "127.0.0.1:1".parse().unwrap();
	assert_eq!(replace(sdp, addr, to), "a=candidate:1 1 udp 2130706431 127.0.0.1 1 typ host\r\n");
}
