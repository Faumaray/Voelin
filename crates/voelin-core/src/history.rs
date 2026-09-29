//! Chat history: every chat message a session sees is stored, and opening a
//! chat shows what is stored, then what the gateway has that we missed.
//!
//! # Contract for the UI
//!
//! A chat's messages are [`HistoryMessage`]s, keyed by
//! [`HistoryMessage::id`] (the local id, stable on this device) and ordered
//! by `(message.ts_ms, id)`. Every [`Event::ChatHistory`] is an upsert of
//! messages into that list:
//!
//! - [`crate::Command::OpenChat`] (and every (re)connect of a source that
//!   tells the server's unique id) emits the newest `chat.history_page`
//!   stored messages at once, [`HistorySource::Local`]. If the session has
//!   a gateway that keeps history (capability `history`), the gateway's
//!   messages we do not have yet follow: everything newer than the chat's
//!   sync cursor (the latest page when the chat was never synced), stored
//!   and emitted in batches of `chat.history_page`, [`HistorySource::Gateway`].
//!   A last `Gateway` batch, possibly empty, says the chat is in sync.
//!   Until one arrives the list holds only what this device saw: label it
//!   so ("only messages seen by this device").
//! - [`crate::Command::LoadOlderHistory`] emits the page before a message:
//!   the gateway's page for that time range is fetched and merged first when
//!   the session has one (`Gateway`), else the stored one (`Local`).
//! - Live messages (voice, gateway pushes, query relays, our own sends) are
//!   stored and emitted one by one as [`HistorySource::Live`]; so are
//!   changes of stored messages (pins, reactions, the gateway id arriving
//!   for a message seen over voice). A copy of a message that is already
//!   there (it arrived over voice and from the gateway) is merged into the
//!   existing row: same id, no second message.
//! - `complete: true`: nothing older than the batch exists as far as the
//!   engine can tell; stop offering to load older messages.
//!
//! [`Event::Chat`] still reports each live message as before (without ids).
//!
//! Merging copies ("dedupe") is described in [`voelin_store::chat`]: the
//! gateway id when known, else the same text (without a relay's `[nick] `
//! prefix), author and time within `chat.dedupe_tolerance_ms`, from
//! different sources.
//!
//! # Storage
//!
//! [`History`] owns a database connection on a writer thread: writes are
//! batched into one transaction per wake-up, statements are cached, and the
//! engine's tasks never wait on the disk. `chat.store_history` (read on
//! every write) off keeps messages in memory for this run only; their ids
//! are negative. `chat.retention_days` prunes old messages (pinned ones
//! stay). Without [`crate::Command::AttachHistory`] the engine keeps its
//! history in memory.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;

use serde::Serialize;
use tokio::sync::{broadcast, oneshot};
use tracing::warn;
use voelin_gateway_proto::{HistoryEntry, HistoryQuery, ReactionCount, feature};
use voelin_model::{ChatMessage, ChatTarget};
pub use voelin_store::MessageSource;
use voelin_store::{ChatCursor, WriteOutcome};
use voelin_store::{NewMessage, PageQuery, Reaction, RemoteInfo, Store, StoredMessage, Written};

use crate::contacts::Contacts;
use crate::gateway::{GatewayClient, GatewayUpdate};
use crate::settings::{CHAT_DEDUPE_TOLERANCE_MS, CHAT_HISTORY_PAGE, CHAT_STORE_HISTORY, Settings};
use crate::{Event, SessionId};

/// A chat message with what the engine knows about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HistoryMessage {
	/// Local id, stable on this device (negative: kept in memory only).
	pub id: i64,
	pub message: ChatMessage,
	/// What delivered it first.
	pub source: MessageSource,
	/// The gateway's id: pin, react and start topics with it.
	pub remote_id: Option<i64>,
	/// The gateway topic the message belongs to.
	pub topic_id: Option<i64>,
	/// In order of the first reaction.
	pub reactions: Vec<ReactionCount>,
	pub pinned: bool,
	/// Gateway revision of the last change (0 without a gateway copy).
	pub rev: i64,
}

/// Where the messages of an [`Event::ChatHistory`] come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
	/// Stored on this device: only what it saw (or fetched before).
	Local,
	/// Fetched from the session's gateway (and stored).
	Gateway,
	/// New or changed just now.
	Live,
}

pub(crate) fn store_target(target: &ChatTarget) -> voelin_store::ChatTarget {
	match target {
		ChatTarget::Server => voelin_store::ChatTarget::Server,
		ChatTarget::Channel(cid) => voelin_store::ChatTarget::Channel(*cid),
		ChatTarget::Private(uid) => voelin_store::ChatTarget::Private(uid.clone()),
	}
}

fn model_target(target: &voelin_store::ChatTarget) -> ChatTarget {
	match target {
		voelin_store::ChatTarget::Server => ChatTarget::Server,
		voelin_store::ChatTarget::Channel(cid) => ChatTarget::Channel(*cid),
		voelin_store::ChatTarget::Private(uid) => ChatTarget::Private(uid.clone()),
	}
}

impl From<StoredMessage> for HistoryMessage {
	fn from(m: StoredMessage) -> Self {
		Self {
			id: m.id,
			message: ChatMessage {
				target: model_target(&m.target),
				author_name: m.author_name,
				author_uid: m.author_uid,
				author_id: m.author_id,
				text: m.text,
				ts_ms: m.ts_ms,
				via_relay: m.via_relay,
				blocked: false,
			},
			source: m.source,
			remote_id: m.remote_id,
			topic_id: m.topic_id,
			reactions: m
				.reactions
				.into_iter()
				.map(|r| ReactionCount { emoji: r.emoji, count: r.count, me: r.me })
				.collect(),
			pinned: m.pinned,
			rev: m.rev,
		}
	}
}

impl HistoryMessage {
	/// A gateway message that could not be stored (local id 0).
	pub(crate) fn unstored(entry: &HistoryEntry) -> Self {
		Self {
			id: 0,
			message: entry.message.clone(),
			source: MessageSource::Gateway,
			remote_id: Some(entry.id),
			topic_id: entry.topic_id,
			reactions: entry.reactions.clone(),
			pinned: entry.pinned,
			rev: entry.rev,
		}
	}
}

/// A live message (voice, query relay, our own) to store.
pub(crate) fn new_message(
	server_uid: &str,
	msg: &ChatMessage,
	source: MessageSource,
) -> NewMessage {
	NewMessage {
		server_uid: server_uid.to_owned(),
		target: store_target(&msg.target),
		ts_ms: msg.ts_ms,
		author_uid: msg.author_uid.clone(),
		author_name: msg.author_name.clone(),
		author_id: msg.author_id,
		via_relay: msg.via_relay,
		text: msg.text.clone(),
		source,
		remote: None,
	}
}

/// A gateway message to store.
pub(crate) fn gateway_message(server_uid: &str, entry: &HistoryEntry) -> NewMessage {
	let mut m = new_message(server_uid, &entry.message, MessageSource::Gateway);
	m.remote = Some(RemoteInfo {
		id: entry.id,
		rev: entry.rev,
		topic_id: entry.topic_id,
		reactions: entry
			.reactions
			.iter()
			.map(|r| Reaction { emoji: r.emoji.clone(), count: r.count, me: r.me })
			.collect(),
		pinned: entry.pinned,
	});
	m
}

type Done<T> = Box<dyn FnOnce(Result<T, String>) + Send>;

enum Job {
	Write { memory: bool, messages: Vec<NewMessage>, tolerance_ms: i64, done: Done<Vec<Written>> },
	Run { memory: bool, f: Box<dyn FnOnce(&mut Store) + Send> },
	Flush(std_mpsc::Sender<()>),
}

struct Inner {
	jobs: Mutex<Option<std_mpsc::Sender<Job>>>,
	thread: Mutex<Option<JoinHandle<()>>>,
	persistent: bool,
}

impl Drop for Inner {
	fn drop(&mut self) {
		// Closing the queue ends the writer after the pending jobs.
		self.jobs.get_mut().unwrap_or_else(PoisonError::into_inner).take();
		if let Some(thread) = self.thread.get_mut().unwrap_or_else(PoisonError::into_inner).take()
			// The last handle may go with a job's callback, on the writer
			// itself: it ends on its own once the queue is empty.
			&& thread.thread().id() != std::thread::current().id()
		{
			let _ = thread.join();
		}
	}
}

/// The chat history database; see the [module docs](self). Cheap to clone.
#[derive(Clone)]
pub struct History {
	inner: Arc<Inner>,
}

impl std::fmt::Debug for History {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("History").field("persistent", &self.inner.persistent).finish()
	}
}

impl Default for History {
	fn default() -> Self {
		Self::in_memory()
	}
}

/// First id of messages kept in memory only (`chat.store_history` off):
/// negative, so they never collide with stored ones.
const MEMORY_IDS: i64 = -(1 << 62);

impl History {
	/// History in the client database at `path` (its own connection).
	pub fn open(path: &Path) -> Result<Self, voelin_store::Error> {
		Ok(Self::start(Store::open(path)?, true))
	}

	/// History kept in memory for this run.
	pub fn in_memory() -> Self {
		let store =
			Store::open_in_memory().and_then(|s| s.start_message_ids_at(MEMORY_IDS).map(|()| s));
		Self::start(store.expect("in-memory database"), false)
	}

	/// History in `store`, which moves to the writer thread.
	pub fn with_store(store: Store) -> Self {
		Self::start(store, true)
	}

	fn start(store: Store, persistent: bool) -> Self {
		let (tx, rx) = std_mpsc::channel();
		let thread = std::thread::Builder::new()
			.name("chat-history".into())
			.spawn(move || writer(store, persistent, rx))
			.expect("history thread");
		Self {
			inner: Arc::new(Inner {
				jobs: Mutex::new(Some(tx)),
				thread: Mutex::new(Some(thread)),
				persistent,
			}),
		}
	}

	/// Whether messages go to a database file (else memory).
	pub fn is_persistent(&self) -> bool {
		self.inner.persistent
	}

	fn send(&self, job: Job) {
		let jobs = self.inner.jobs.lock().unwrap_or_else(PoisonError::into_inner);
		if let Some(tx) = jobs.as_ref() {
			let _ = tx.send(job);
		}
	}

	/// Wait until everything queued so far is done (blocking).
	pub fn flush(&self) {
		let (tx, rx) = std_mpsc::channel();
		self.send(Job::Flush(tx));
		let _ = rx.recv();
	}

	/// Queue messages; `done` runs on the writer thread after they are stored.
	pub(crate) fn write(
		&self,
		memory: bool,
		messages: Vec<NewMessage>,
		tolerance_ms: i64,
		done: impl FnOnce(Result<Vec<Written>, String>) + Send + 'static,
	) {
		self.send(Job::Write { memory, messages, tolerance_ms, done: Box::new(done) });
	}

	pub(crate) async fn write_async(
		&self,
		memory: bool,
		messages: Vec<NewMessage>,
		tolerance_ms: i64,
	) -> Result<Vec<Written>, String> {
		let (tx, rx) = oneshot::channel();
		self.write(memory, messages, tolerance_ms, move |r| {
			let _ = tx.send(r);
		});
		rx.await.map_err(|_| "chat history stopped".to_string())?
	}

	/// Run `f` on the writer thread, after the jobs queued before.
	pub(crate) async fn run<T: Send + 'static>(
		&self,
		memory: bool,
		f: impl FnOnce(&mut Store) -> voelin_store::Result<T> + Send + 'static,
	) -> Result<T, String> {
		let (tx, rx) = oneshot::channel();
		self.send(Job::Run {
			memory,
			f: Box::new(move |store| {
				let _ = tx.send(f(store).map_err(|e| e.to_string()));
			}),
		});
		rx.await.map_err(|_| "chat history stopped".to_string())?
	}

	/// Run `f` on the writer thread, then `then` with its result (there too).
	pub(crate) fn run_then<T: Send + 'static>(
		&self,
		memory: bool,
		f: impl FnOnce(&mut Store) -> voelin_store::Result<T> + Send + 'static,
		then: impl FnOnce(Result<T, String>) + Send + 'static,
	) {
		self.send(Job::Run {
			memory,
			f: Box::new(move |store| then(f(store).map_err(|e| e.to_string()))),
		});
	}

	/// Stored messages of a chat, oldest first (from the database; with
	/// `memory`, from what was kept in memory while `chat.store_history`
	/// was off).
	pub async fn page(
		&self,
		server_uid: &str,
		target: &ChatTarget,
		query: PageQuery,
		memory: bool,
	) -> Result<Vec<HistoryMessage>, String> {
		let (uid, target) = (server_uid.to_owned(), store_target(target));
		let rows = self.run(memory, move |s| s.messages(&uid, &target, query)).await?;
		Ok(rows.into_iter().map(HistoryMessage::from).collect())
	}

	/// Delete messages older than `before_ms` (pinned ones stay).
	pub(crate) fn prune(&self, before_ms: i64) {
		self.send(Job::Run {
			memory: false,
			f: Box::new(move |store| {
				if let Err(e) = store.prune_messages(before_ms) {
					warn!("cannot prune chat history: {e}");
				}
			}),
		});
	}
}

/// The writer thread: runs jobs in order; consecutive writes to the same
/// database go into one transaction.
fn writer(store: Store, persistent: bool, rx: std_mpsc::Receiver<Job>) {
	let mut main = store;
	// For `chat.store_history` off while the main store is a file.
	let mut ephemeral: Option<Store> = None;
	while let Ok(first) = rx.recv() {
		let mut queue: VecDeque<Job> = std::iter::once(first).chain(rx.try_iter()).collect();
		while let Some(job) = queue.pop_front() {
			match job {
				Job::Write { memory, messages, tolerance_ms, done } => {
					let mut batch = vec![(messages, done)];
					while let Some(Job::Write { memory: m, tolerance_ms: t, .. }) = queue.front()
						&& *m == memory && *t == tolerance_ms
					{
						let Some(Job::Write { messages, done, .. }) = queue.pop_front() else {
							unreachable!()
						};
						batch.push((messages, done));
					}
					let store = pick(&mut main, &mut ephemeral, persistent, memory);
					let all: Vec<NewMessage> =
						batch.iter().flat_map(|(m, _)| m.iter().cloned()).collect();
					match store.write_messages(&all, tolerance_ms) {
						Ok(written) => {
							let mut written = written.into_iter();
							for (messages, done) in batch {
								done(Ok(written.by_ref().take(messages.len()).collect()));
							}
						}
						Err(e) => {
							warn!("cannot store chat messages: {e}");
							for (_, done) in batch {
								done(Err(e.to_string()));
							}
						}
					}
				}
				Job::Run { memory, f } => f(pick(&mut main, &mut ephemeral, persistent, memory)),
				Job::Flush(done) => {
					let _ = done.send(());
				}
			}
		}
	}
}

/// The store a job uses: the main one, or with `memory` while the main one
/// is a file, one in memory.
fn pick<'a>(
	main: &'a mut Store,
	ephemeral: &'a mut Option<Store>,
	persistent: bool,
	memory: bool,
) -> &'a mut Store {
	if !(memory && persistent) {
		return main;
	}
	ephemeral.get_or_insert_with(|| {
		let store = Store::open_in_memory().expect("in-memory database");
		if let Err(e) = store.start_message_ids_at(MEMORY_IDS) {
			warn!("in-memory chat ids: {e}");
		}
		store
	})
}

/// The engine's history: one [`History`] that
/// [`crate::Command::AttachHistory`] can replace, shared by the sessions.
#[derive(Clone, Debug, Default)]
pub(crate) struct SharedHistory(Arc<RwLock<History>>);

impl SharedHistory {
	pub fn new(history: History) -> Self {
		Self(Arc::new(RwLock::new(history)))
	}

	pub fn current(&self) -> History {
		self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
	}

	pub fn replace(&self, history: History) {
		*self.0.write().unwrap_or_else(PoisonError::into_inner) = history;
	}
}

/// What the chat history flows of one session need.
#[derive(Clone)]
pub(crate) struct ChatCtx {
	pub session: SessionId,
	pub events: broadcast::Sender<Event>,
	pub history: History,
	pub settings: Settings,
	/// The key of the server's history.
	pub server_uid: String,
	/// Messages of blocked contacts are flagged.
	pub contacts: Contacts,
}

impl ChatCtx {
	/// `chat.store_history` off: memory only.
	pub fn memory(&self) -> bool {
		!self.settings.get(&CHAT_STORE_HISTORY)
	}

	/// `chat.history_page`; `None`: no limit.
	pub fn page_limit(&self) -> Option<u32> {
		Some(self.settings.get(&CHAT_HISTORY_PAGE)).filter(|n| *n > 0)
	}

	pub fn tolerance_ms(&self) -> i64 {
		self.settings.get(&CHAT_DEDUPE_TOLERANCE_MS).min(i64::MAX as u64) as i64
	}

	pub fn emit(&self, event: Event) {
		let _ = self.events.send(event);
	}

	pub fn emit_batch(
		&self,
		target: &ChatTarget,
		messages: Vec<HistoryMessage>,
		source: HistorySource,
		complete: bool,
	) {
		let mut messages = messages;
		for m in &mut messages {
			m.message.blocked = self.contacts.is_blocked(m.message.author_uid.as_deref());
		}
		self.emit(Event::ChatHistory {
			session: self.session,
			target: target.clone(),
			messages,
			source,
			complete,
		});
	}

	fn gateway_failed(&self, request: &str, error: &crate::gateway::ClientError) {
		self.emit(Event::Gateway {
			session: self.session,
			update: GatewayUpdate::Failed {
				request: request.to_owned(),
				code: error.code(),
				message: error.to_string(),
			},
		});
	}

	/// Store live messages; each new or changed one is emitted as
	/// [`HistorySource::Live`] from the writer thread.
	pub fn store_live(&self, messages: Vec<NewMessage>) {
		let (session, events) = (self.session, self.events.clone());
		let contacts = self.contacts.clone();
		self.history.write(self.memory(), messages, self.tolerance_ms(), move |result| {
			for w in result.unwrap_or_default() {
				if w.outcome == WriteOutcome::Unchanged {
					continue;
				}
				let mut message = HistoryMessage::from(w.message);
				message.message.blocked =
					contacts.is_blocked(message.message.author_uid.as_deref());
				let _ = events.send(Event::ChatHistory {
					session,
					target: message.message.target.clone(),
					messages: vec![message],
					source: HistorySource::Live,
					complete: false,
				});
			}
		});
	}

	/// Store gateway entries; returns the stored rows in input order.
	pub async fn store_entries(&self, entries: &[HistoryEntry]) -> Result<Vec<Written>, String> {
		let messages = entries.iter().map(|e| gateway_message(&self.server_uid, e)).collect();
		self.history.write_async(self.memory(), messages, self.tolerance_ms()).await
	}

	/// Emit a stored message that changed (e.g. its pin), as `Live`.
	pub fn emit_changed(&self, message: Option<StoredMessage>) {
		if let Some(m) = message {
			let m = HistoryMessage::from(m);
			let target = m.message.target.clone();
			self.emit_batch(&target, vec![m], HistorySource::Live, false);
		}
	}
}

/// Whether a gateway keeps the history of `target` (it relays server and
/// channel chat, not private chat).
pub(crate) fn gateway_history(client: &GatewayClient, target: &ChatTarget) -> bool {
	!matches!(target, ChatTarget::Private(_)) && client.has(feature::HISTORY)
}

/// A chat opened (or its session reconnected): the stored page, then what
/// the gateway has that we do not.
pub(crate) async fn open_chat(
	ctx: ChatCtx,
	target: ChatTarget,
	gateway: Option<GatewayClient>,
	gateway_pending: bool,
) {
	let gateway = gateway.filter(|g| gateway_history(g, &target));
	let limit = ctx.page_limit();
	let query = PageQuery { limit: limit.map(|l| l as usize), ..Default::default() };
	match ctx.history.page(&ctx.server_uid, &target, query, ctx.memory()).await {
		Ok(local) => {
			let short = limit.is_none_or(|l| local.len() < l as usize);
			let complete = short && gateway.is_none() && !gateway_pending;
			ctx.emit_batch(&target, local, HistorySource::Local, complete);
		}
		Err(e) => warn!("cannot read chat history: {e}"),
	}
	if let Some(client) = gateway {
		sync_chat(&ctx, &client, &target).await;
	}
}

/// Fetch what the gateway has for `target` beyond the chat's cursor.
async fn sync_chat(ctx: &ChatCtx, client: &GatewayClient, target: &ChatTarget) {
	let (memory, uid, key) = (ctx.memory(), ctx.server_uid.clone(), store_target(target));
	let gateway_id = client.info().gateway_id.clone();
	let cursor = {
		let (uid, key) = (uid.clone(), key.clone());
		ctx.history.run(memory, move |s| s.chat_cursor(&uid, &key)).await.ok().flatten()
	};
	let mut cursor = match cursor {
		Some(c) if c.gateway_id == gateway_id => Some(c),
		// Another gateway numbers messages differently.
		Some(_) => {
			let (uid, key) = (uid.clone(), key.clone());
			let _ = ctx.history.run(memory, move |s| s.forget_remote_ids(&uid, &key)).await;
			None
		}
		None => None,
	};
	let limit = ctx.page_limit();
	let save = |cursor: ChatCursor| {
		let (uid, key) = (uid.clone(), key.clone());
		ctx.history.run(memory, move |s| s.set_chat_cursor(&uid, &key, &cursor))
	};
	let mut complete = cursor.as_ref().is_some_and(|c| c.complete);
	if cursor.is_none() {
		// Never synced: the latest page; older ones when scrolling back.
		let page = match client.history(HistoryQuery::latest(target.clone(), limit)).await {
			Ok(page) => page,
			Err(e) => return ctx.gateway_failed("history", &e),
		};
		complete = !page.has_more;
		let rev = page.messages.iter().map(|e| e.rev).max().unwrap_or(0);
		match ctx.store_entries(&page.messages).await {
			Ok(written) => {
				let messages = written.into_iter().map(|w| w.message.into()).collect();
				ctx.emit_batch(target, messages, HistorySource::Gateway, complete);
			}
			Err(e) => return warn!("cannot store chat history: {e}"),
		}
		let c = ChatCursor { gateway_id: gateway_id.clone(), rev, complete, updated_ms: now_ms() };
		let _ = save(c.clone()).await;
		cursor = Some(c);
	}
	let mut rev = cursor.map_or(0, |c| c.rev);
	loop {
		let (entries, next, has_more) = match client.sync(target.clone(), rev, limit).await {
			Ok(page) => page,
			Err(e) => return ctx.gateway_failed("sync", &e),
		};
		let changed: Vec<HistoryMessage> = match ctx.store_entries(&entries).await {
			Ok(written) => written
				.into_iter()
				.filter(|w| w.outcome != WriteOutcome::Unchanged)
				.map(|w| w.message.into())
				.collect(),
			Err(e) => return warn!("cannot store chat history: {e}"),
		};
		rev = rev.max(next);
		let c = ChatCursor { gateway_id: gateway_id.clone(), rev, complete, updated_ms: now_ms() };
		let _ = save(c).await;
		if !has_more || entries.is_empty() {
			// The last batch, possibly empty: in sync.
			return ctx.emit_batch(target, changed, HistorySource::Gateway, complete);
		}
		if !changed.is_empty() {
			ctx.emit_batch(target, changed, HistorySource::Gateway, false);
		}
	}
}

/// The page before the message with local id `before` (the newest page
/// without it): the gateway's page for that range merged in first.
pub(crate) async fn load_older(
	ctx: ChatCtx,
	target: ChatTarget,
	before: Option<i64>,
	gateway: Option<GatewayClient>,
) {
	let gateway = gateway.filter(|g| gateway_history(g, &target));
	let (memory, uid, key) = (ctx.memory(), ctx.server_uid.clone(), store_target(&target));
	let anchor = match before {
		Some(id) => ctx
			.history
			.run(memory, move |s| s.message(id))
			.await
			.ok()
			.flatten()
			.map(|m| (m.ts_ms, m.id)),
		None => None,
	};
	let limit = ctx.page_limit();
	// Whether the gateway has nothing older (None: not asked).
	let mut gateway_complete = None;
	if let Some(client) = &gateway {
		let gateway_id = client.info().gateway_id.clone();
		let (cursor, oldest) = {
			let (uid, key) = (uid.clone(), key.clone());
			ctx.history
				.run(memory, move |s| {
					Ok((s.chat_cursor(&uid, &key)?, s.oldest_remote_ts(&uid, &key)?))
				})
				.await
				.unwrap_or((None, None))
		};
		let cursor = cursor.filter(|c| c.gateway_id == gateway_id);
		let known_complete = cursor.as_ref().is_some_and(|c| c.complete)
			&& anchor.is_some_and(|(ts, _)| oldest.is_some_and(|o| ts <= o));
		if known_complete {
			gateway_complete = Some(true);
		} else {
			let mut query = HistoryQuery::latest(target.clone(), limit);
			// Up to and including the anchor's millisecond (copies merge).
			query.before_ms = anchor.map(|(ts, _)| ts.saturating_add(1));
			match client.history(query).await {
				Ok(page) => {
					if let Err(e) = ctx.store_entries(&page.messages).await {
						warn!("cannot store chat history: {e}");
					}
					gateway_complete = Some(!page.has_more);
					// The gateway's oldest message is stored now.
					if let Some(mut c) = cursor.filter(|_| !page.has_more) {
						c.complete = true;
						let (uid, key) = (uid.clone(), key.clone());
						let _ = ctx
							.history
							.run(memory, move |s| s.set_chat_cursor(&uid, &key, &c))
							.await;
					}
				}
				Err(e) => ctx.gateway_failed("history", &e),
			}
		}
	}
	let query = PageQuery { before: anchor, after: None, limit: limit.map(|l| l as usize) };
	match ctx.history.page(&uid, &target, query, memory).await {
		Ok(local) => {
			let short = limit.is_none_or(|l| local.len() < l as usize);
			let complete = short && gateway_complete.unwrap_or(gateway.is_none());
			let source = if gateway_complete.is_some() {
				HistorySource::Gateway
			} else {
				HistorySource::Local
			};
			ctx.emit_batch(&target, local, source, complete);
		}
		Err(e) => warn!("cannot read chat history: {e}"),
	}
}

pub(crate) fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or_default()
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc as std_mpsc;

	use super::*;

	fn live(text: &str, ts_ms: i64) -> NewMessage {
		let msg = ChatMessage {
			target: ChatTarget::Channel(1),
			author_name: "a".into(),
			author_uid: Some("uid-a".into()),
			author_id: None,
			text: text.into(),
			ts_ms,
			via_relay: false,
			blocked: false,
		};
		new_message("srv", &msg, MessageSource::Voice)
	}

	/// Writes queued together run in one batch, each answered with its own
	/// rows, in order; reads see them.
	#[tokio::test]
	async fn writes_are_batched_and_answered_in_order() {
		let path = std::env::temp_dir()
			.join(format!("voelin-history-{}", std::process::id()))
			.join("client.db");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		let history = History::open(&path).unwrap();
		let (tx, rx) = std_mpsc::channel();
		for i in 0..50 {
			let tx = tx.clone();
			let batch = vec![live(&format!("m{i}"), i), live(&format!("n{i}"), i)];
			history.write(false, batch, 5000, move |r| {
				let texts: Vec<String> = r.unwrap().into_iter().map(|w| w.message.text).collect();
				tx.send((i, texts)).unwrap();
			});
		}
		history.flush();
		let answers: Vec<_> = rx.try_iter().collect();
		assert_eq!(answers.len(), 50);
		for (n, (i, texts)) in answers.into_iter().enumerate() {
			assert_eq!(i, n as i64);
			assert_eq!(texts, [format!("m{i}"), format!("n{i}")]);
		}
		let page = history
			.page(
				"srv",
				&ChatTarget::Channel(1),
				PageQuery { limit: Some(2), ..Default::default() },
				false,
			)
			.await
			.unwrap();
		assert_eq!(
			page.iter().map(|m| m.message.text.as_str()).collect::<Vec<_>>(),
			["m49", "n49"]
		);
		assert!(page[0].id > 0);
		// Kept in memory only: negative ids, not in the file.
		let kept = history.write_async(true, vec![live("secret", 99)], 5000).await.unwrap();
		assert!(kept[0].message.id < 0);
		let stored =
			history.page("srv", &ChatTarget::Channel(1), PageQuery::default(), false).await;
		assert_eq!(stored.unwrap().len(), 100);
		drop(history);
		std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}
}
