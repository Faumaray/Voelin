//! The voice source: a normal client connection.
//!
//! Besides presence, chat, voice and stream signalling it carries what only
//! a client connection can do: pokes, file transfer (channel files,
//! avatars, icons) and offline messages. Their answers go straight to the
//! engine's event bus ([`VoiceLink`]); what the session must see first
//! (chat, pokes: blocking) comes as [`VoiceEvent`]s.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures::prelude::*;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::{Instant, timeout};
use tracing::{debug, info, warn};
use tsclientlib::events::Event as BookEvent;
use tsclientlib::messages::c2s;
use tsclientlib::prelude::*;
use tsclientlib::{
	Connection, DisconnectOptions, FiletransferHandle, Identity, InMessage, MessageHandle,
	MessageTarget, StreamItem, TsError, Version,
};
use tsproto_packets::packets::{AudioData, InAudioBuf, OutCommand, OutPacket};
use voelin_model::{ChannelId, ChatMessage, ChatTarget, Presence, ServerFlavor};
use voelin_stream::{PeerConfig, Request, StreamNotification};

use crate::book::presence_from_book;
use crate::cache;
use crate::files::{self, FileEntry, Report, RequestId, Sink, TransferId, TransferState};
use crate::offline::{OfflineMessage, OfflineMessageInfo};
use crate::settings::{FILES_PROGRESS_MS, SharedSettings};
use crate::{Event, SessionId};

#[derive(Clone, Debug)]
pub struct VoiceOptions {
	pub address: String,
	pub nickname: String,
	pub identity: Option<Identity>,
	/// Signed compatibility version; `None` selects the native platform's tuple.
	pub client_version: Option<Version>,
	pub server_password: Option<String>,
	/// Channel path to join, e.g. `Lobby/Sub`.
	pub channel: Option<String>,
	/// Open the audio devices (capture and playback).
	pub audio: bool,
	/// Network settings for stream peer connections (TeamSpeak 6).
	pub stream_peer: PeerConfig,
}

impl VoiceOptions {
	pub fn new(address: impl Into<String>, nickname: impl Into<String>) -> Self {
		Self {
			address: address.into(),
			nickname: nickname.into(),
			identity: None,
			client_version: None,
			server_password: None,
			channel: None,
			audio: false,
			stream_peer: PeerConfig::default(),
		}
	}
}

/// Where a voice connection reports answers the session does not need to see.
#[derive(Clone)]
pub(crate) struct VoiceLink {
	pub myts_identity: watch::Receiver<Option<Arc<tsproto::myts::Identity>>>,
	/// What the server is shown of the account ([`crate::Command::SetMytsData`]).
	pub myts_data: watch::Receiver<Option<Arc<tsclientlib::MytsData>>>,
	pub session: SessionId,
	pub events: broadcast::Sender<Event>,
	pub settings: SharedSettings,
}

impl VoiceLink {
	fn emit(&self, event: Event) {
		let _ = self.events.send(event);
	}

	fn done(&self, request: RequestId, result: Result<(), String>) {
		self.emit(Event::RequestDone { session: self.session, request, result });
	}
}

/// A file on the server: channel, its password, path.
#[derive(Clone, Debug)]
pub(crate) struct Remote {
	pub channel: ChannelId,
	pub password: Option<String>,
	pub path: String,
}

impl Remote {
	fn cpw(&self) -> String {
		encode_password(self.password.as_deref())
	}
}

fn encode_password(password: Option<&str>) -> String {
	password
		.filter(|p| !p.is_empty())
		.map(|p| tsproto_types::crypto::encode_password(p.as_bytes()))
		.unwrap_or_default()
}

pub(crate) enum VoiceCmd {
	SendChat(ChatTarget, String),
	Move(ChannelId, Option<String>),
	SetInputMuted(bool),
	SetOutputMuted(bool),
	/// An encoded voice packet from the audio thread.
	Audio(OutPacket),
	/// A stream command; failures come back as [`VoiceEvent::StreamRequestFailed`].
	Stream(Request),
	Poke {
		client: u16,
		message: String,
	},
	ListFiles {
		request: RequestId,
		dir: Remote,
	},
	/// Download `file`; `transfer` if the user can cancel it.
	Download {
		transfer: Option<TransferId>,
		file: Remote,
		sink: Sink,
		report: Report,
	},
	Upload {
		transfer: Option<TransferId>,
		file: Remote,
		from: PathBuf,
		overwrite: bool,
		resume: bool,
		report: Report,
	},
	CancelTransfer(TransferId),
	DeleteFiles {
		/// `None`: best effort, nobody waits for the answer.
		request: Option<RequestId>,
		channel: ChannelId,
		password: Option<String>,
		paths: Vec<String>,
	},
	RenameFile {
		request: RequestId,
		from: Remote,
		to: Remote,
	},
	CreateDirectory {
		request: RequestId,
		dir: Remote,
	},
	/// `clientupdate client_flag_avatar` (empty: no avatar).
	SetAvatarHash {
		request: Option<RequestId>,
		hash: String,
	},
	OfflineList {
		request: RequestId,
	},
	OfflineGet {
		request: RequestId,
		id: u32,
	},
	OfflineAdd {
		request: RequestId,
		to_uid: String,
		subject: String,
		text: String,
	},
	OfflineDelete {
		request: RequestId,
		id: u32,
	},
	OfflineFlag {
		request: RequestId,
		id: u32,
		read: bool,
	},
	Disconnect,
}

pub(crate) enum VoiceEvent {
	Connected {
		name: String,
		flavor: ServerFlavor,
		own_client: u16,
		/// The server's unique id (from its public key, as the server
		/// generation computes it).
		server_uid: String,
		/// Our unique id as the server knows it.
		own_uid: Option<String>,
	},
	Presence(Box<Presence>),
	OwnChannel(ChannelId),
	Chat(ChatMessage),
	Poke {
		from: u16,
		from_uid: Option<String>,
		from_name: String,
		message: String,
	},
	/// Incoming voice for the audio thread, and who is talking.
	Audio(InAudioBuf),
	Talking {
		client: u16,
		talking: bool,
	},
	Stream(StreamNotification),
	StreamRequestFailed(Request, String),
	/// A command failed (the connection stays).
	Error(String),
	Disconnected(Option<String>),
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or_default()
}

enum Input {
	MytsIdentityChanged,
	MytsDataChanged,
	AccountSourceClosed,
	Item(Option<Result<StreamItem, tsclientlib::Error>>),
	Cmd(Option<VoiceCmd>),
	Tick,
}

/// A command waiting for the server's answer.
enum Pending {
	MytsIdentity(Instant),
	/// `updatemytsdata`: its answer is only logged.
	MytsData,
	Stream(Request),
	FileList {
		request: RequestId,
		channel: ChannelId,
		path: String,
		entries: Vec<FileEntry>,
	},
	Done(RequestId),
	OfflineList {
		request: RequestId,
		messages: Vec<OfflineMessageInfo>,
	},
	OfflineGet {
		request: RequestId,
		id: u32,
		message: Option<OfflineMessage>,
	},
	/// Failures are reported as [`VoiceEvent::Error`] with this label.
	Report(&'static str),
	/// Best effort: the answer does not matter.
	Quiet,
}

/// A transfer the server has not opened yet.
enum Job {
	Download {
		transfer: Option<TransferId>,
		offset: u64,
		sink: Sink,
		report: Report,
		started: Instant,
	},
	Upload {
		transfer: Option<TransferId>,
		from: PathBuf,
		size: u64,
		report: Report,
	},
}

impl Job {
	fn report(&self) -> &Report {
		match self {
			Job::Download { report, .. } | Job::Upload { report, .. } => report,
		}
	}

	fn transfer(&self) -> Option<TransferId> {
		match self {
			Job::Download { transfer, .. } | Job::Upload { transfer, .. } => *transfer,
		}
	}
}

/// How long a picture of `size` bytes may take to arrive: a minute, plus
/// its size at [`cache::MIN_PICTURE_RATE`], so large banners arrive on slow
/// links while a stalled transfer still ends.
fn image_download_time(size: u64) -> Duration {
	Duration::from_secs(60 + size / cache::MIN_PICTURE_RATE)
}

/// How long the server may take to start an image transfer (a busy server
/// answers the requests of a large channel tree in turn).
const IMAGE_NEGOTIATION: Duration = Duration::from_secs(30);

/// Bound background image handshakes without imposing a deadline on user transfers.
fn expire_image_jobs(jobs: &mut HashMap<FiletransferHandle, Job>, now: Instant) {
	jobs.retain(|_, job| {
		if matches!(job, Job::Download { transfer: None, started, .. } if now.duration_since(*started) >= IMAGE_NEGOTIATION) {
			job.report()(TransferState::Failed("image transfer negotiation timed out".into()));
			false
		} else {
			true
		}
	});
}

/// A user's transfer, for cancelling.
enum Slot {
	Waiting(FiletransferHandle),
	Running { task: AbortHandle, part: Option<PathBuf>, report: Report },
}

/// Run a voice connection until it ends or is told to disconnect.
pub(crate) async fn run(
	options: VoiceOptions,
	link: VoiceLink,
	mut commands: mpsc::UnboundedReceiver<VoiceCmd>,
	events: mpsc::UnboundedSender<VoiceEvent>,
) {
	let reason = match run_inner(&options, link, &mut commands, &events).await {
		Ok(()) => None,
		Err(e) => Some(e.to_string()),
	};
	let _ = events.send(VoiceEvent::Disconnected(reason));
}

/// The state of a connected voice source.
struct Voice {
	con: Connection,
	link: VoiceLink,
	events: mpsc::UnboundedSender<VoiceEvent>,
	server_uid: String,
	/// Who is talking: last voice packet per client.
	talking: HashMap<u16, Instant>,
	pending: HashMap<MessageHandle, Pending>,
	jobs: HashMap<FiletransferHandle, Job>,
	transfers: HashMap<TransferId, Slot>,
	/// `updatemytsdata` is to be sent once connected again: a reconnect
	/// (tsclientlib's own) leaves the server without it.
	myts_data_due: bool,
}

async fn run_inner(
	options: &VoiceOptions,
	link: VoiceLink,
	commands: &mut mpsc::UnboundedReceiver<VoiceCmd>,
	events: &mpsc::UnboundedSender<VoiceEvent>,
) -> anyhow::Result<()> {
	let version = match &options.client_version {
		Some(version) => version.clone(),
		None => crate::versions::native_version()?,
	};
	let mut builder = Connection::build(options.address.clone())
		.name(options.nickname.clone())
		.version(version)
		.metadata(crate::versions::client_metadata());
	if let Some(identity) = &options.identity {
		builder = builder.identity(identity.clone());
	}
	if let Some(pw) = &options.server_password {
		builder = builder.password(pw.clone());
	}
	if let Some(channel) = &options.channel {
		builder = builder.channel(channel.clone());
	}
	let mut identity = link.myts_identity.clone();
	let mut data = link.myts_data.clone();
	// A changed account invalidates an in-flight handshake. Drop that connection
	// and take a fresh snapshot before publishing any connected state.
	let mut con = timeout(Duration::from_secs(30), async {
		loop {
			let snapshot = identity.borrow_and_update().as_deref().cloned();
			let mut con = builder.clone().myts_identity(snapshot).connect()?;
			let connected = tokio::select! {
				biased;
				changed = identity.changed() => {
					changed.map_err(|_| anyhow::anyhow!("account source closed"))?;
					continue;
				}
				connected = async {
					con.events()
						.try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
						.next().await
				} => connected,
			};
			match connected {
				Some(Ok(_)) => {}
				Some(Err(e)) => return Err(anyhow::Error::from(e)),
				None => anyhow::bail!("connection closed"),
			}
			if identity.has_changed().unwrap_or(true) {
				// Connected already: leave properly, or the server keeps a
				// client with the old account until it times out, next to
				// the new one.
				let _ = shutdown_connection(con).await;
				continue;
			}
			return Ok(con);
		}
	})
	.await
	.map_err(|_| anyhow::anyhow!("timed out while connecting"))??;
	let server_uid = {
		let state = con.get_state()?;
		let flavor = ServerFlavor::from_version_string(&state.server.version);
		let ids = voelin_gateway_proto::UniqueIds::from_omega(&state.server.public_key.to_ts());
		let server_uid = ids.for_server(matches!(flavor, ServerFlavor::Ts6(_))).to_owned();
		info!(server = %state.server.name, ?flavor, %server_uid, "voice connected");
		let own_uid = state
			.clients
			.get(&state.own_client)
			.and_then(|c| c.uid.as_ref())
			.map(|u| u.as_ref().to_string());
		let _ = events.send(VoiceEvent::Connected {
			name: state.server.name.clone(),
			flavor,
			own_client: state.own_client.0,
			server_uid: server_uid.clone(),
			own_uid,
		});
		server_uid
	};
	// Subscribe to all channels to see everyone.
	let cmd = con.get_state()?.server.set_subscribed(true);
	cmd.send(&mut con)?;
	let mut voice = Voice {
		con,
		link,
		events: events.clone(),
		server_uid,
		talking: HashMap::new(),
		pending: HashMap::new(),
		jobs: HashMap::new(),
		transfers: HashMap::new(),
		myts_data_due: false,
	};
	voice.publish_state()?;
	// The account's avatar, for everyone on the server (as the official
	// client, once connected).
	data.borrow_and_update();
	voice.send_myts_data();

	let mut tick = tokio::time::interval(Duration::from_millis(250));
	let result = loop {
		// Check on every turn so busy command/audio queues cannot delay expiry.
		if voice.pending.values().any(|pending| {
			matches!(pending, Pending::MytsIdentity(started)
				if started.elapsed() >= Duration::from_secs(10))
		}) {
			break Err(anyhow::anyhow!("myTeamSpeak account update timed out"));
		}
		let input =
			next_input(&mut voice.con.events(), commands, &mut tick, &mut identity, &mut data)
				.await;
		match input {
			Input::AccountSourceClosed => break Err(anyhow::anyhow!("account source closed")),
			Input::MytsIdentityChanged => {
				let snapshot = identity.borrow_and_update().as_deref().cloned();
				match voice.con.update_myts_identity(snapshot) {
					Ok(handle) => {
						voice.pending.insert(handle, Pending::MytsIdentity(Instant::now()));
					}
					Err(e) => break Err(anyhow::anyhow!("myTeamSpeak account update: {e}")),
				}
				// After the proof, what is shown of the account (a sign-in):
				// its change, if any, is in this one.
				data.borrow_and_update();
				voice.send_myts_data();
			}
			Input::MytsDataChanged => {
				data.borrow_and_update();
				voice.send_myts_data();
			}
			Input::Item(None) => break Err(anyhow::anyhow!("connection closed")),
			Input::Item(Some(Err(e))) => break Err(e.into()),
			Input::Item(Some(Ok(item))) => {
				if let Err(e) = voice.item(item) {
					break Err(e);
				}
			}
			Input::Cmd(None) | Input::Cmd(Some(VoiceCmd::Disconnect)) => break Ok(()),
			Input::Cmd(Some(cmd)) => {
				if let Err(e) = voice.command(cmd) {
					let _ = voice.events.send(VoiceEvent::Error(e.to_string()));
				}
			}
			Input::Tick => voice.tick(),
		}
	};
	// Transfers the server never opened fail; running ones go on.
	for (_, job) in voice.jobs.drain() {
		job.report()(TransferState::Failed("disconnected".into()));
	}
	let disconnect = shutdown_connection(voice.con).await;
	result?;
	disconnect?;
	Ok(())
}

async fn next_input(
	stream: &mut (impl Stream<Item = Result<StreamItem, tsclientlib::Error>> + Unpin),
	commands: &mut mpsc::UnboundedReceiver<VoiceCmd>,
	tick: &mut tokio::time::Interval,
	identity: &mut watch::Receiver<Option<Arc<tsproto::myts::Identity>>>,
	data: &mut watch::Receiver<Option<Arc<tsclientlib::MytsData>>>,
) -> Input {
	tokio::select! {
		biased;
		changed = identity.changed() => if changed.is_ok() {
			Input::MytsIdentityChanged
		} else {
			Input::AccountSourceClosed
		},
		changed = data.changed() => if changed.is_ok() {
			Input::MytsDataChanged
		} else {
			Input::AccountSourceClosed
		},
		input = async {
			// Keep account changes first without starving network or timers when
			// the audio command queue remains ready.
			tokio::select! {
				cmd = commands.recv() => Input::Cmd(cmd),
				_ = tick.tick() => Input::Tick,
				item = stream.next() => Input::Item(item),
			}
		} => input,
	}
}

async fn shutdown_connection(mut con: Connection) -> Result<(), tsclientlib::Error> {
	// disconnect() also returns Ok while Connecting. Never poll that stale
	// handshake: dropping it is the cancellation boundary for logout.
	if con.get_state().is_err() {
		return Ok(());
	}
	con.disconnect(DisconnectOptions::new())?;
	let _ = timeout(Duration::from_secs(3), async {
		while con.get_state().is_ok() {
			if con.events().next().await.is_none() {
				break;
			}
		}
	})
	.await;
	Ok(())
}

impl Voice {
	fn publish_state(&self) -> anyhow::Result<()> {
		let state = self.con.get_state()?;
		let talking = |id| self.talking.contains_key(&id);
		let presence = presence_from_book(state, Some(&self.server_uid), talking);
		let _ = self.events.send(VoiceEvent::Presence(Box::new(presence)));
		if let Some(own) = state.clients.get(&state.own_client) {
			let _ = self.events.send(VoiceEvent::OwnChannel(own.channel.0));
		}
		Ok(())
	}

	fn tick(&mut self) {
		expire_image_jobs(&mut self.jobs, Instant::now());
		let events = &self.events;
		self.talking.retain(|client, last| {
			let active = last.elapsed() < Duration::from_millis(400);
			if !active {
				let _ = events.send(VoiceEvent::Talking { client: *client, talking: false });
			}
			active
		});
	}

	fn progress_every(&self) -> Duration {
		Duration::from_millis(self.link.settings.current().get(&FILES_PROGRESS_MS).into())
	}

	fn item(&mut self, item: StreamItem) -> anyhow::Result<()> {
		match item {
			StreamItem::BookEvents(book_events) => {
				for e in &book_events {
					if let BookEvent::Message { target, invoker, message } = e {
						self.message(target, invoker, message);
					}
				}
				// Connected again after a reconnect.
				if self.myts_data_due {
					self.send_myts_data();
				}
				self.publish_state()?;
			}
			StreamItem::Audio(packet) => {
				let from = match packet.data().data() {
					AudioData::S2C { from, .. } | AudioData::S2CWhisper { from, .. } => *from,
					_ => return Ok(()),
				};
				if self.talking.insert(from, Instant::now()).is_none() {
					let _ = self.events.send(VoiceEvent::Talking { client: from, talking: true });
				}
				let _ = self.events.send(VoiceEvent::Audio(packet));
			}
			StreamItem::MessageEvent(msg) => self.message_event(&msg),
			StreamItem::MessageResult(handle, result) => {
				if let Some(pending) = self.pending.remove(&handle) {
					if matches!(pending, Pending::MytsIdentity(_)) {
						result.map_err(|e| {
							anyhow::anyhow!("myTeamSpeak account update: {}", e.error)
						})?;
					} else {
						self.answered(pending, result.map_err(|e| e.error));
					}
				}
			}
			StreamItem::FileDownload(handle, download) => {
				if let Some(Job::Download { transfer, offset, sink, report, .. }) =
					self.jobs.remove(&handle)
				{
					// Pictures (avatars, icons, banners): bounded like the
					// banners on the web, the user's own files are not.
					if transfer.is_none() && download.size > cache::MAX_PICTURE_BYTES {
						let limit = cache::MAX_PICTURE_BYTES >> 20;
						report(TransferState::Failed(format!("larger than {limit} MiB")));
						return Ok(());
					}
					let part = sink.part().map(ToOwned::to_owned);
					let progress = self.progress_every();
					let task_report = report.clone();
					let deadline = image_download_time(download.size);
					let task = tokio::spawn(async move {
						let receive = files::download(
							download.stream,
							download.size,
							offset,
							sink,
							progress,
							task_report.clone(),
						);
						if transfer.is_some() {
							receive.await;
						} else if timeout(deadline, receive).await.is_err() {
							task_report(TransferState::Failed("image download timed out".into()));
						}
					});
					self.running(transfer, task.abort_handle(), part, report);
				}
			}
			StreamItem::FileUpload(handle, upload) => {
				if let Some(Job::Upload { transfer, from, size, report }) =
					self.jobs.remove(&handle)
				{
					let task = tokio::spawn(files::upload(
						upload.stream,
						from,
						size,
						upload.seek_position,
						self.progress_every(),
						report.clone(),
					));
					self.running(transfer, task.abort_handle(), None, report);
				}
			}
			StreamItem::FiletransferFailed(handle, error) => {
				if let Some(job) = self.jobs.remove(&handle) {
					if let Some(id) = job.transfer() {
						self.transfers.remove(&id);
					}
					job.report()(TransferState::Failed(transfer_error(&error)));
				}
			}
			StreamItem::DisconnectedTemporarily(reason) => {
				warn!(?reason, "voice connection interrupted, reconnecting");
				self.myts_data_due = true;
			}
			_ => {}
		}
		Ok(())
	}

	fn running(
		&mut self,
		transfer: Option<TransferId>,
		task: AbortHandle,
		part: Option<PathBuf>,
		report: Report,
	) {
		self.transfers.retain(|_, slot| match slot {
			Slot::Running { task, .. } => !task.is_finished(),
			Slot::Waiting(_) => true,
		});
		if let Some(id) = transfer {
			self.transfers.insert(id, Slot::Running { task, part, report });
		}
	}

	/// A chat message or poke from the book.
	fn message(&self, target: &MessageTarget, invoker: &tsclientlib::Invoker, text: &str) {
		let uid = invoker.uid.as_ref().map(|u| u.as_ref().to_string());
		let target = match target {
			MessageTarget::Poke(_) => {
				let _ = self.events.send(VoiceEvent::Poke {
					from: invoker.id.0,
					from_uid: uid,
					from_name: invoker.name.to_string(),
					message: text.to_owned(),
				});
				return;
			}
			MessageTarget::Server => ChatTarget::Server,
			MessageTarget::Channel => ChatTarget::Channel(self.own_channel().unwrap_or(0)),
			// Our own private message comes back to us: it belongs to the
			// chat with its receiver.
			MessageTarget::Client(to) if Some(invoker.id) == self.own_client() => {
				ChatTarget::Private(self.client_uid(to).unwrap_or_default())
			}
			MessageTarget::Client(_) => ChatTarget::Private(uid.clone().unwrap_or_default()),
		};
		let _ = self.events.send(VoiceEvent::Chat(ChatMessage {
			target,
			author_name: invoker.name.to_string(),
			author_uid: uid,
			author_id: Some(invoker.id.0),
			text: text.to_owned(),
			ts_ms: now_ms(),
			via_relay: false,
			blocked: false,
		}));
	}

	/// Messages the book does not handle: stream signalling, file lists,
	/// offline messages.
	fn message_event(&mut self, msg: &InMessage) {
		match msg {
			InMessage::FileList(list) => {
				for part in list.iter() {
					let path = files::normalize_dir(&part.path);
					let entry = FileEntry {
						name: part.name.clone(),
						size: part.size,
						modified_s: part.date_time.unix_timestamp(),
						is_dir: !part.is_file,
					};
					let channel = part.channel_id.0;
					if let Some(Pending::FileList { entries, .. }) = self.first_pending(|p| {
						matches!(p, Pending::FileList { channel: c, path: p, .. }
							if *c == channel && *p == path)
					}) {
						entries.push(entry);
					}
				}
			}
			InMessage::OfflineMessageList(list) => {
				for part in list.iter() {
					let info = OfflineMessageInfo {
						id: part.message_id,
						from_uid: part.client_uid.as_ref().to_string(),
						subject: part.subject.clone(),
						ts_s: part.timestamp.unix_timestamp(),
						read: part.is_read,
					};
					if let Some(Pending::OfflineList { messages, .. }) =
						self.first_pending(|p| matches!(p, Pending::OfflineList { .. }))
					{
						messages.push(info);
					}
				}
			}
			InMessage::OfflineMessage(list) => {
				for part in list.iter() {
					let m = OfflineMessage {
						id: part.message_id,
						from_uid: part.client_uid.as_ref().to_string(),
						subject: part.subject.clone(),
						text: part.message.clone(),
						ts_s: part.timestamp.unix_timestamp(),
					};
					let id = part.message_id;
					if let Some(Pending::OfflineGet { message, .. }) = self.first_pending(
						|p| matches!(p, Pending::OfflineGet { id: i, .. } if *i == id),
					) {
						*message = Some(m);
					}
				}
			}
			_ => {
				for n in StreamNotification::from_message(msg) {
					let _ = self.events.send(VoiceEvent::Stream(n));
				}
			}
		}
	}

	/// The oldest pending command matching `f` (answers come in order).
	fn first_pending(&mut self, f: impl Fn(&Pending) -> bool) -> Option<&mut Pending> {
		let key = self.pending.iter().filter(|(_, p)| f(p)).map(|(h, _)| *h).min_by_key(|h| h.0)?;
		self.pending.get_mut(&key)
	}

	/// The server answered a command.
	fn answered(&mut self, pending: Pending, result: Result<(), TsError>) {
		let session = self.link.session;
		// An empty list is an empty result, not an error.
		let listed = match result {
			Ok(()) | Err(TsError::DatabaseEmptyResult) => Ok(()),
			Err(e) => Err(e),
		};
		match pending {
			Pending::Stream(request) => {
				if let Err(e) = result {
					let _ =
						self.events.send(VoiceEvent::StreamRequestFailed(request, e.to_string()));
				}
			}
			Pending::FileList { request, channel, path, entries } => {
				let result = listed.map(|()| entries).map_err(|e| e.to_string());
				self.link.emit(Event::FileList { session, request, channel, path, result });
			}
			Pending::Done(request) => self.link.done(request, result.map_err(|e| e.to_string())),
			Pending::OfflineList { request, messages } => {
				let result = listed.map(|()| messages).map_err(|e| e.to_string());
				self.link.emit(Event::OfflineMessages { session, request, result });
			}
			Pending::OfflineGet { request, id, message } => {
				let result = match (result, message) {
					(Ok(()), Some(m)) => Ok(m),
					(Ok(()), None) => Err(format!("offline message {id} not found")),
					(Err(e), _) => Err(e.to_string()),
				};
				self.link.emit(Event::OfflineMessage { session, request, result });
			}
			Pending::Quiet | Pending::MytsIdentity(_) => {}
			Pending::MytsData => match result {
				Ok(()) => debug!("the server took the account's avatar"),
				Err(e) => warn!(error = %e, "the server refused the account's avatar"),
			},
			Pending::Report(what) => {
				if let Err(e) = result {
					let _ = self.events.send(VoiceEvent::Error(format!("{what}: {e}")));
				}
			}
		}
	}

	/// Show the server the account's certificate and avatar
	/// (`updatemytsdata`), when connected with the account; while
	/// reconnecting, once connected again.
	fn send_myts_data(&mut self) {
		self.myts_data_due = false;
		if self.link.myts_identity.borrow().is_none() {
			return;
		}
		let data = self.link.myts_data.borrow().clone();
		let Some(data) = data.filter(|d| !d.certificate.is_empty()) else {
			return;
		};
		match self.con.send_myts_data(&data) {
			Ok(handle) => {
				self.pending.insert(handle, Pending::MytsData);
			}
			Err(tsclientlib::Error::NotConnected) => self.myts_data_due = true,
			Err(e) => warn!(error = %e, "the account's avatar was not sent"),
		}
	}

	fn send(&mut self, cmd: OutCommand, pending: Pending) -> anyhow::Result<()> {
		let handle = cmd.send_with_result(&mut self.con)?;
		self.pending.insert(handle, pending);
		Ok(())
	}

	fn command(&mut self, cmd: VoiceCmd) -> anyhow::Result<()> {
		match cmd {
			VoiceCmd::SendChat(target, text) => {
				let state = self.con.get_state()?;
				let out = match target {
					ChatTarget::Server => state.server.send_textmessage(&text),
					ChatTarget::Channel(_) => state.send_message(MessageTarget::Channel, &text),
					ChatTarget::Private(uid) => {
						let client = state
							.clients
							.values()
							.find(|c| c.uid.as_ref().is_some_and(|u| u.as_ref().to_string() == uid))
							.ok_or_else(|| {
								anyhow::anyhow!(
									"private message: the client is not on the server \
									 (send an offline message instead)"
								)
							})?;
						state.send_message(MessageTarget::Client(client.id), &text)
					}
				};
				self.send(out, Pending::Report("message"))?;
			}
			VoiceCmd::Move(channel, password) => {
				let state = self.con.get_state()?;
				let own = &state.clients[&state.own_client];
				let mut part = own.client_move(tsclientlib::ChannelId(channel));
				if let Some(pw) = &password {
					part = part.set_password(pw);
				}
				part.send(&mut self.con)?;
			}
			VoiceCmd::SetInputMuted(muted) => {
				self.con.get_state()?.client_update().set_input_muted(muted).send(&mut self.con)?;
			}
			VoiceCmd::SetOutputMuted(muted) => {
				let update = self.con.get_state()?.client_update().set_output_muted(muted);
				update.send(&mut self.con)?;
			}
			VoiceCmd::Audio(packet) => self.con.send_audio(packet)?,
			VoiceCmd::Stream(request) => {
				let cmd = request.to_command();
				self.send(cmd, Pending::Stream(request))?;
			}
			VoiceCmd::Poke { client, message } => {
				let cmd = c2s::OutClientPokeRequestMessage::new(&mut std::iter::once(
					c2s::OutClientPokeRequestPart {
						client_id: tsclientlib::ClientId(client),
						message: message.as_str().into(),
					},
				));
				self.send(cmd, Pending::Report("poke"))?;
			}
			VoiceCmd::ListFiles { request, dir } => {
				let path = files::normalize_dir(&dir.path);
				let cpw = dir.cpw();
				let cmd = c2s::OutFileListRequestMessage::new(&mut std::iter::once(
					c2s::OutFileListRequestPart {
						channel_id: tsclientlib::ChannelId(dir.channel),
						channel_password: cpw.as_str().into(),
						path: path.as_str().into(),
					},
				));
				let pending =
					Pending::FileList { request, channel: dir.channel, path, entries: Vec::new() };
				self.send(cmd, pending)?;
			}
			VoiceCmd::Download { transfer, file, sink, report } => {
				let offset = sink.offset();
				let seek = (offset > 0).then_some(offset);
				let password = file.password.as_deref().filter(|p| !p.is_empty());
				let channel = tsclientlib::ChannelId(file.channel);
				let handle = match self.con.download_file(channel, &file.path, password, seek) {
					Ok(handle) => handle,
					Err(e) => {
						report(TransferState::Failed(e.to_string()));
						return Ok(());
					}
				};
				report(TransferState::Requested);
				self.jobs.insert(
					handle,
					Job::Download { transfer, offset, sink, report, started: Instant::now() },
				);
				if let Some(id) = transfer {
					self.transfers.insert(id, Slot::Waiting(handle));
				}
			}
			VoiceCmd::Upload { transfer, file, from, overwrite, resume, report } => {
				let size = match std::fs::metadata(&from) {
					Ok(m) if m.is_file() => m.len(),
					Ok(_) => {
						report(TransferState::Failed(format!("{} is not a file", from.display())));
						return Ok(());
					}
					Err(e) => {
						report(TransferState::Failed(format!("{}: {e}", from.display())));
						return Ok(());
					}
				};
				let password = file.password.as_deref().filter(|p| !p.is_empty());
				let channel = tsclientlib::ChannelId(file.channel);
				let handle = match self
					.con
					.upload_file(channel, &file.path, password, size, overwrite, resume)
				{
					Ok(handle) => handle,
					Err(e) => {
						report(TransferState::Failed(e.to_string()));
						return Ok(());
					}
				};
				report(TransferState::Requested);
				self.jobs.insert(handle, Job::Upload { transfer, from, size, report });
				if let Some(id) = transfer {
					self.transfers.insert(id, Slot::Waiting(handle));
				}
			}
			VoiceCmd::CancelTransfer(id) => match self.transfers.remove(&id) {
				Some(Slot::Waiting(handle)) => {
					if let Some(job) = self.jobs.remove(&handle) {
						job.report()(TransferState::Cancelled);
					}
				}
				Some(Slot::Running { task, part, report }) if !task.is_finished() => {
					task.abort();
					if let Some(part) = part {
						let _ = std::fs::remove_file(part);
					}
					report(TransferState::Cancelled);
				}
				_ => debug!(id, "cancel: no such transfer (finished?)"),
			},
			VoiceCmd::DeleteFiles { request, channel, password, paths } => {
				let cpw = encode_password(password.as_deref());
				let mut parts = paths.iter().map(|p| c2s::OutDeleteFilePart {
					channel_id: tsclientlib::ChannelId(channel),
					channel_password: cpw.as_str().into(),
					name: p.as_str().into(),
				});
				let cmd = c2s::OutDeleteFileMessage::new(&mut parts);
				self.send(cmd, request.map_or(Pending::Quiet, Pending::Done))?;
			}
			VoiceCmd::RenameFile { request, from, to } => {
				let (cpw, tcpw) = (from.cpw(), to.cpw());
				let other = to.channel != from.channel;
				let cmd =
					c2s::OutRenameFileMessage::new(&mut std::iter::once(c2s::OutRenameFilePart {
						channel_id: tsclientlib::ChannelId(from.channel),
						channel_password: cpw.as_str().into(),
						target_channel_id: other.then_some(tsclientlib::ChannelId(to.channel)),
						target_channel_password: other.then_some(tcpw.as_str().into()),
						old_name: from.path.as_str().into(),
						new_name: to.path.as_str().into(),
					}));
				self.send(cmd, Pending::Done(request))?;
			}
			VoiceCmd::CreateDirectory { request, dir } => {
				let cpw = dir.cpw();
				let cmd = c2s::OutCreateDirectoryMessage::new(&mut std::iter::once(
					c2s::OutCreateDirectoryPart {
						channel_id: tsclientlib::ChannelId(dir.channel),
						channel_password: cpw.as_str().into(),
						directory_name: dir.path.as_str().into(),
					},
				));
				self.send(cmd, Pending::Done(request))?;
			}
			VoiceCmd::SetAvatarHash { request, hash } => {
				let update = self.con.get_state()?.client_update().set_avatar_hash(&hash);
				let cmd = c2s::OutClientUpdateMessage::new(&mut std::iter::once(update));
				let pending = request.map_or(Pending::Report("avatar"), Pending::Done);
				self.send(cmd, pending)?;
			}
			VoiceCmd::OfflineList { request } => {
				let cmd = c2s::OutOfflineMessageListRequestMessage::new();
				self.send(cmd, Pending::OfflineList { request, messages: Vec::new() })?;
			}
			VoiceCmd::OfflineGet { request, id } => {
				let cmd = c2s::OutOfflineMessageGetMessage::new(&mut std::iter::once(
					c2s::OutOfflineMessageGetPart { message_id: id },
				));
				self.send(cmd, Pending::OfflineGet { request, id, message: None })?;
			}
			VoiceCmd::OfflineAdd { request, to_uid, subject, text } => {
				// The command carries the unique id's bytes (base64 on the wire).
				let bytes = base64::engine::general_purpose::STANDARD
					.decode(to_uid.trim())
					.map_err(|_| anyhow::anyhow!("offline message: invalid unique id {to_uid:?}"));
				let uid = match bytes {
					Ok(bytes) => tsclientlib::UidBuf(bytes),
					Err(e) => {
						self.link.done(request, Err(e.to_string()));
						return Ok(());
					}
				};
				let cmd = c2s::OutOfflineMessageAddMessage::new(&mut std::iter::once(
					c2s::OutOfflineMessageAddPart {
						client_uid: std::borrow::Cow::Borrowed(uid.as_ref()),
						subject: subject.as_str().into(),
						message: text.as_str().into(),
					},
				));
				self.send(cmd, Pending::Done(request))?;
			}
			VoiceCmd::OfflineDelete { request, id } => {
				let cmd = c2s::OutOfflineMessageDelMessage::new(&mut std::iter::once(
					c2s::OutOfflineMessageDelPart { message_id: id },
				));
				self.send(cmd, Pending::Done(request))?;
			}
			VoiceCmd::OfflineFlag { request, id, read } => {
				let cmd = c2s::OutOfflineMessageUpdateFlagMessage::new(&mut std::iter::once(
					c2s::OutOfflineMessageUpdateFlagPart { message_id: id, is_read: read },
				));
				self.send(cmd, Pending::Done(request))?;
			}
			VoiceCmd::Disconnect => {}
		}
		Ok(())
	}

	fn own_client(&self) -> Option<tsclientlib::ClientId> {
		self.con.get_state().ok().map(|state| state.own_client)
	}

	fn client_uid(&self, client: &tsclientlib::ClientId) -> Option<String> {
		let state = self.con.get_state().ok()?;
		state.clients.get(client)?.uid.as_ref().map(|u| u.as_ref().to_string())
	}

	fn own_channel(&self) -> Option<ChannelId> {
		let state = self.con.get_state().ok()?;
		state.clients.get(&state.own_client).map(|c| c.channel.0)
	}
}

/// A failed transfer, for the user.
fn transfer_error(error: &tsclientlib::Error) -> String {
	match error {
		tsclientlib::Error::CommandError(e) if e.error == TsError::FileNotFound => {
			"file not found".into()
		}
		e => e.to_string(),
	}
}

#[cfg(test)]
mod image_download_tests {
	use super::*;
	use std::sync::{Arc, Mutex};

	#[test]
	fn stalled_image_handshake_expires_but_user_transfer_does_not() {
		let started = Instant::now();
		let reports = Arc::new(Mutex::new(Vec::new()));
		let sink = reports.clone();
		let report: Report = Arc::new(move |state| sink.lock().unwrap().push(state));
		let mut jobs = HashMap::new();
		for (id, transfer) in [(1, None), (2, Some(77))] {
			jobs.insert(
				FiletransferHandle(id),
				Job::Download {
					transfer,
					offset: 0,
					sink: Sink::Memory,
					report: report.clone(),
					started,
				},
			);
		}
		expire_image_jobs(&mut jobs, started + IMAGE_NEGOTIATION - Duration::from_secs(1));
		assert_eq!(jobs.len(), 2);
		assert!(reports.lock().unwrap().is_empty());
		expire_image_jobs(&mut jobs, started + IMAGE_NEGOTIATION);
		assert_eq!(jobs.len(), 1);
		assert!(jobs.contains_key(&FiletransferHandle(2)));
		assert!(
			matches!(&reports.lock().unwrap()[..], [TransferState::Failed(error)] if error.contains("timed out"))
		);
		expire_image_jobs(&mut jobs, started + Duration::from_secs(60));
		assert_eq!(reports.lock().unwrap().len(), 1);
	}

	#[test]
	fn large_pictures_get_time_to_arrive() {
		assert_eq!(image_download_time(0), Duration::from_secs(60));
		assert_eq!(image_download_time(100 << 10), Duration::from_secs(72));
		// The largest picture, at the slowest rate allowed.
		assert_eq!(image_download_time(cache::MAX_PICTURE_BYTES), Duration::from_secs(60 + 16384));
	}
}

#[cfg(test)]
mod account_tests {
	use super::*;

	#[tokio::test(start_paused = true)]
	async fn account_priority_preserves_ready_network_and_timer_progress() {
		let (account, mut identity) = watch::channel(None);
		let (account_data, mut data) = watch::channel(None);
		let (commands, mut receiver) = mpsc::unbounded_channel();
		for _ in 0..512 {
			commands.send(VoiceCmd::SetInputMuted(false)).unwrap();
		}
		let mut stream = futures::stream::poll_fn(|_| {
			std::task::Poll::Ready(Some(Err(tsclientlib::Error::NotConnected)))
		});
		let mut tick = tokio::time::interval(Duration::from_millis(1));
		account.send_replace(None);
		assert!(matches!(
			next_input(&mut stream, &mut receiver, &mut tick, &mut identity, &mut data).await,
			Input::MytsIdentityChanged
		));
		identity.borrow_and_update();
		// What is shown of the account changes too, ahead of the rest.
		account_data.send_replace(Some(Arc::new(tsclientlib::MytsData::default())));
		assert!(matches!(
			next_input(&mut stream, &mut receiver, &mut tick, &mut identity, &mut data).await,
			Input::MytsDataChanged
		));
		data.borrow_and_update();
		let (mut network, mut timers, mut audio) = (0, 0, 0);
		for _ in 0..512 {
			tokio::time::advance(Duration::from_millis(1)).await;
			match next_input(&mut stream, &mut receiver, &mut tick, &mut identity, &mut data).await
			{
				Input::Item(_) => network += 1,
				Input::Tick => timers += 1,
				Input::Cmd(_) => audio += 1,
				_ => panic!("unexpected account change"),
			}
		}
		assert!(network > 0 && timers > 0 && audio > 0);
	}

	/// A reconnect (tsclientlib's own) leaves the server without the
	/// account's avatar: it goes again once connected, not before.
	#[tokio::test]
	async fn account_data_waits_for_the_connection() {
		// A silent UDP peer: never connected.
		let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		// Scalar one and the standard compressed Edwards base point.
		let mut private = [0; 32];
		private[0] = 1;
		let mut public = [0x66; 32];
		public[0] = 0x58;
		let identity =
			tsproto::myts::Identity::new(vec![1; 33], 42, public, private, vec![2; 65]).unwrap();
		let data = tsclientlib::MytsData { certificate: vec![1], avatar: Vec::new() };
		let (events, _) = broadcast::channel(8);
		let link = VoiceLink {
			session: 1,
			events,
			settings: SharedSettings::new(crate::settings::Settings::default()),
			myts_identity: watch::channel(Some(Arc::new(identity))).1,
			myts_data: watch::channel(Some(Arc::new(data))).1,
		};
		let mut voice = Voice {
			con: Connection::build(server.local_addr().unwrap().to_string()).connect().unwrap(),
			link,
			events: mpsc::unbounded_channel().0,
			server_uid: String::new(),
			talking: HashMap::new(),
			pending: HashMap::new(),
			jobs: HashMap::new(),
			transfers: HashMap::new(),
			myts_data_due: false,
		};
		voice.send_myts_data();
		assert!(voice.myts_data_due && voice.pending.is_empty());
		voice.myts_data_due = false;
		let reconnecting = tsclientlib::TemporaryDisconnectReason::Timeout("test");
		voice.item(StreamItem::DisconnectedTemporarily(reconnecting)).unwrap();
		assert!(voice.myts_data_due);
		// Not connected yet: still due.
		let _ = voice.item(StreamItem::BookEvents(Vec::new()));
		assert!(voice.myts_data_due && voice.pending.is_empty());
		timeout(Duration::from_millis(100), shutdown_connection(voice.con)).await.unwrap().unwrap();
	}

	#[tokio::test]
	async fn shutdown_never_polls_a_connecting_connection() {
		let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let mut con =
			Connection::build(server.local_addr().unwrap().to_string()).connect().unwrap();
		assert!(matches!(con.update_myts_identity(None), Err(tsclientlib::Error::NotConnected)));
		// tsclientlib returns Ok here despite retaining its Connecting future.
		assert!(con.disconnect(DisconnectOptions::new()).is_ok());
		timeout(Duration::from_millis(100), shutdown_connection(con)).await.unwrap().unwrap();
		let mut packet = [0; 2048];
		assert!(matches!(server.try_recv_from(&mut packet), Err(error)
			if error.kind() == std::io::ErrorKind::WouldBlock));
	}

	#[tokio::test]
	async fn account_change_cancels_pending_handshake() {
		// A silent UDP peer keeps the handshake in flight without an external server.
		let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let (account, identity) = watch::channel(None);
		let (events, _) = broadcast::channel(8);
		let link = VoiceLink {
			session: 1,
			events,
			settings: SharedSettings::new(crate::settings::Settings::default()),
			myts_identity: identity,
			myts_data: watch::channel(None).1,
		};
		let (_commands, commands) = mpsc::unbounded_channel();
		let (events, mut received) = mpsc::unbounded_channel();
		let task = tokio::spawn(run(
			VoiceOptions::new(server.local_addr().unwrap().to_string(), "account-test"),
			link,
			commands,
			events,
		));
		let mut packet = [0; 2048];
		let (_, first) =
			timeout(Duration::from_secs(5), server.recv_from(&mut packet)).await.unwrap().unwrap();
		// A logout/clear notification must cancel the old socket and restart.
		account.send_replace(None);
		let second = timeout(Duration::from_secs(5), async {
			loop {
				let (_, peer) = server.recv_from(&mut packet).await.unwrap();
				if peer != first {
					break peer;
				}
			}
		})
		.await
		.unwrap();
		assert_ne!(first, second);
		// Losing the account authority fails closed immediately, not after connect timeout.
		drop(account);
		timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
		assert!(
			matches!(received.recv().await, Some(VoiceEvent::Disconnected(Some(reason))) if reason == "account source closed")
		);
		assert!(received.try_recv().is_err());
	}
}
