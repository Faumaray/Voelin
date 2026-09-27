//! Finding streams the server did not announce to us.
//!
//! The server sends `notifystreamstarted` only to the clients that are in the
//! streamer's channel when the stream starts. A client that connects later, or
//! enters the channel later, only sees the streamer's `client_is_streaming=1`.
//! `requeststreaminfo clid=<streamer>` answers with a `notifystreaminfo` part
//! per stream of that client, for any client on the server (see
//! `docs/research/ts6-late-join.md`).
//!
//! [`Discovery`] follows the clients on the server (their channel and
//! streaming flag, fed by [`Streams::update_clients`](crate::Streams::update_clients)),
//! keeps the [`StreamDirectory`] to the streams in our channel, and looks up
//! the streams of clients that stream in our channel but whose stream we were
//! not told: after connecting, after we or they change channels, and when a
//! streamer's announcement did not come. Where it looks them up is a
//! [`StreamLookup`]: the server ([`ServerLookup`]) now; another directory
//! (e.g. a gateway's) can be added with
//! [`Streams::add_lookup`](crate::Streams::add_lookup) and hands what it finds
//! to [`Streams::discovered`](crate::Streams::discovered).

use std::collections::{BTreeMap, BTreeSet};

use tracing::debug;
use tsclientlib::ClientId;

use crate::session::{Outbox, Request, StreamDirectory};

/// What discovery needs to know about a client on the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientState {
	pub channel: u64,
	/// `client_is_streaming`; `None` if the server did not say (TeamSpeak 3).
	pub streaming: Option<bool>,
}

/// A place to look up the streams of a client.
pub trait StreamLookup: Send {
	/// Start looking up the streams of `streamer`; `false` if this source
	/// cannot (then the next one is asked). The server's answer arrives as
	/// `notifystreaminfo`; other sources report through
	/// [`Streams::discovered`](crate::Streams::discovered).
	fn lookup(&mut self, streamer: ClientId, out: &mut Outbox) -> bool;

	/// A request of this source failed on the server.
	fn failed(&mut self, _request: &Request, _error: &str) {}
}

/// Asks the server: `requeststreaminfo clid=<streamer>`.
#[derive(Debug, Default)]
pub struct ServerLookup {
	/// The server does not know the command (older TeamSpeak 6 versions).
	unsupported: bool,
}

impl StreamLookup for ServerLookup {
	fn lookup(&mut self, streamer: ClientId, out: &mut Outbox) -> bool {
		if self.unsupported {
			return false;
		}
		out.request(Request::StreamInfo { streamer });
		true
	}

	fn failed(&mut self, request: &Request, error: &str) {
		// "command not found" (error 256): never ask again on this connection.
		if error.contains("CommandNotFound") || error.contains("command not found") {
			debug!(?request, "the server does not know requeststreaminfo");
			self.unsupported = true;
		}
	}
}

/// See the [module docs](self).
pub struct Discovery {
	own: ClientId,
	/// The clients as of the last update; empty before the first.
	clients: BTreeMap<u16, ClientState>,
	/// Streamers looked up since they (or we) last changed channel or
	/// started streaming; asked again after such a change.
	asked: BTreeSet<u16>,
	lookups: Vec<Box<dyn StreamLookup>>,
}

impl std::fmt::Debug for Discovery {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Discovery")
			.field("own", &self.own)
			.field("clients", &self.clients.len())
			.field("asked", &self.asked)
			.field("lookups", &self.lookups.len())
			.finish()
	}
}

impl Discovery {
	/// Discovery through the server.
	pub fn new(own: ClientId) -> Self {
		Self {
			own,
			clients: BTreeMap::new(),
			asked: BTreeSet::new(),
			lookups: vec![Box::new(ServerLookup::default())],
		}
	}

	/// Another place to look streams up, asked after the ones before.
	pub fn add_lookup(&mut self, lookup: Box<dyn StreamLookup>) {
		self.lookups.push(lookup);
	}

	/// Our channel, once the clients are known.
	pub fn own_channel(&self) -> Option<u64> {
		self.clients.get(&self.own.0).map(|c| c.channel)
	}

	/// The channel of a client, as of the last update.
	pub fn channel_of(&self, client: ClientId) -> Option<u64> {
		self.clients.get(&client.0).map(|c| c.channel)
	}

	/// Whether a stream of `streamer` belongs in the directory: it is in our
	/// channel (or the clients are not known yet).
	pub fn in_our_channel(&self, streamer: ClientId) -> bool {
		match self.own_channel() {
			None => true,
			Some(own) => self.clients.get(&streamer.0).is_some_and(|c| c.channel == own),
		}
	}

	/// The clients on the server changed (full list). Updates the streaming
	/// flags in `directory`, drops the streams of clients that left or are
	/// not in our channel, and looks up the streams we were not told.
	/// Returns whether the directory's streams changed.
	pub fn update(
		&mut self,
		clients: BTreeMap<u16, ClientState>,
		directory: &mut StreamDirectory,
		out: &mut Outbox,
	) -> bool {
		let old_channel = self.own_channel();
		let new_channel = clients.get(&self.own.0).map(|c| c.channel);
		let mut changed = false;
		// Streams announced to us but whose streamer went away or out of our channel.
		let first = self.clients.is_empty();
		for (id, now) in &clients {
			let was = self.clients.get(id);
			match (was.and_then(|c| c.streaming), now.streaming) {
				// Only changes count: a list may predate the streaming flag of
				// a stream that was just announced.
				(Some(was), Some(now)) if was != now => {
					changed |= directory.set_streaming(ClientId(*id), now);
				}
				(None, Some(true)) => {
					directory.set_streaming(ClientId(*id), true);
				}
				_ => {}
			}
			if was.is_none_or(|was| was.channel != now.channel || was.streaming != now.streaming) {
				self.asked.remove(id);
			}
		}
		if old_channel != new_channel {
			self.asked.clear();
		}
		self.asked.retain(|id| clients.contains_key(id));
		changed |= directory.retain_streamers(|c| {
			clients.get(&c.0).is_some_and(|s| new_channel.is_none_or(|own| s.channel == own))
		});

		// A client in our channel that just started streaming is announced
		// by the server itself; look it up only if the announcement has not
		// come by the next update.
		let just_started = |id: &u16, now: &ClientState| {
			let was = self.clients.get(id);
			!first
				&& was.is_some_and(|was| {
					was.streaming != Some(true) && Some(was.channel) == old_channel
				}) && Some(now.channel) == new_channel
		};
		let candidates: Vec<u16> = clients
			.iter()
			.filter(|(id, c)| {
				**id != self.own.0
					&& c.streaming == Some(true)
					&& new_channel.is_some_and(|own| c.channel == own)
					&& directory.by_streamer(ClientId(**id)).is_none()
					&& !self.asked.contains(id)
					&& !just_started(id, c)
			})
			.map(|(id, _)| *id)
			.collect();
		self.clients = clients;
		for id in candidates {
			let streamer = ClientId(id);
			if self.lookups.iter_mut().any(|l| l.lookup(streamer, out)) {
				debug!(streamer = id, "looking up an unannounced stream");
				self.asked.insert(id);
			}
		}
		changed
	}

	/// A request of a lookup failed on the server.
	pub fn failed(&mut self, request: &Request, error: &str) {
		for lookup in &mut self.lookups {
			lookup.failed(request, error);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::session::Output;
	use crate::{StreamInfo, StreamKind, StreamNotification};

	const OWN: u16 = 1;

	fn clients(list: &[(u16, u64, bool)]) -> BTreeMap<u16, ClientState> {
		list.iter()
			.map(|&(id, channel, streaming)| {
				(id, ClientState { channel, streaming: Some(streaming) })
			})
			.collect()
	}

	fn looked_up(out: &mut Outbox) -> Vec<u16> {
		let mut ids = Vec::new();
		while let Some(o) = out.pop() {
			match o {
				Output::Request(Request::StreamInfo { streamer }) => ids.push(streamer.0),
				other => panic!("unexpected {other:?}"),
			}
		}
		ids
	}

	fn started(id: &str, streamer: u16) -> StreamNotification {
		StreamNotification::Started {
			info: StreamInfo {
				id: id.into(),
				streamer: ClientId(streamer),
				name: String::new(),
				kind: StreamKind::Screen,
				bitrate: 4608,
				viewer_limit: 0,
				audio: true,
			},
			return_code: None,
		}
	}

	#[test]
	fn looks_up_streams_that_were_not_announced() {
		let mut d = Discovery::new(ClientId(OWN));
		let mut dir = StreamDirectory::default();
		let mut out = Outbox::default();

		// Connected: 2 streams in our channel, 3 in another one.
		d.update(clients(&[(OWN, 1, false), (2, 1, true), (3, 5, true)]), &mut dir, &mut out);
		assert_eq!(looked_up(&mut out), [2]);
		// Asked once, not again on the next update.
		d.update(
			clients(&[(OWN, 1, false), (2, 1, true), (3, 5, true), (4, 1, false)]),
			&mut dir,
			&mut out,
		);
		assert_eq!(looked_up(&mut out), [] as [u16; 0]);
		// The answer fills the directory.
		assert!(dir.apply(&started("s-2", 2)));

		// We move to 3's channel: 3 is looked up, 2's stream leaves the list.
		let moved = clients(&[(OWN, 5, false), (2, 1, true), (3, 5, true), (4, 1, false)]);
		assert!(d.update(moved.clone(), &mut dir, &mut out));
		assert_eq!(looked_up(&mut out), [3]);
		assert!(dir.get("s-2").is_none());
		assert!(d.in_our_channel(ClientId(3)) && !d.in_our_channel(ClientId(2)));

		// 4 comes to our channel and starts streaming there: announced by
		// the server, so not looked up at once...
		let mut now = moved.clone();
		now.insert(4, ClientState { channel: 5, streaming: Some(false) });
		d.update(now.clone(), &mut dir, &mut out);
		now.insert(4, ClientState { channel: 5, streaming: Some(true) });
		d.update(now.clone(), &mut dir, &mut out);
		assert_eq!(looked_up(&mut out), [] as [u16; 0]);
		// ...only if the announcement has not come by the next update.
		now.insert(9, ClientState { channel: 1, streaming: Some(false) });
		d.update(now.clone(), &mut dir, &mut out);
		assert_eq!(looked_up(&mut out), [4]);

		// A streamer that stops loses its stream; one that left too.
		assert!(dir.apply(&started("s-4", 4)));
		now.insert(4, ClientState { channel: 5, streaming: Some(false) });
		assert!(d.update(now.clone(), &mut dir, &mut out));
		assert!(dir.get("s-4").is_none());
		assert!(dir.apply(&started("s-3", 3)));
		now.remove(&3);
		assert!(d.update(now, &mut dir, &mut out));
		assert_eq!(dir.iter().count(), 0);
	}

	#[test]
	fn a_server_without_the_command_is_not_asked_again() {
		let mut d = Discovery::new(ClientId(OWN));
		let mut dir = StreamDirectory::default();
		let mut out = Outbox::default();
		d.update(clients(&[(OWN, 1, false), (2, 1, true)]), &mut dir, &mut out);
		let request = Request::StreamInfo { streamer: ClientId(2) };
		assert_eq!(looked_up(&mut out), [2]);
		d.failed(&request, "command failed: CommandNotFound");
		d.update(clients(&[(OWN, 1, false), (3, 1, true)]), &mut dir, &mut out);
		assert_eq!(looked_up(&mut out), [] as [u16; 0]);
	}

	/// A second source, e.g. a gateway's directory.
	struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<u16>>>);

	impl StreamLookup for Recorder {
		fn lookup(&mut self, streamer: ClientId, _out: &mut Outbox) -> bool {
			self.0.lock().unwrap().push(streamer.0);
			true
		}
	}

	#[test]
	fn other_lookups_follow_the_server() {
		let mut d = Discovery::new(ClientId(OWN));
		let asked = std::sync::Arc::default();
		d.add_lookup(Box::new(Recorder(std::sync::Arc::clone(&asked))));
		let mut dir = StreamDirectory::default();
		let mut out = Outbox::default();
		d.failed(&Request::StreamInfo { streamer: ClientId(2) }, "command not found");
		d.update(clients(&[(OWN, 1, false), (2, 1, true)]), &mut dir, &mut out);
		assert_eq!(looked_up(&mut out), [] as [u16; 0]);
		assert_eq!(*asked.lock().unwrap(), [2]);
	}
}
