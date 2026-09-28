//! Per-application audio capture on PipeWire, by linking playback streams
//! into capture streams of ours.
//!
//! A [`LinkManager`] keeps one PipeWire connection on a thread of its own
//! and watches the registry: clients (process ids and executables), nodes
//! of class `Stream/Output/Audio` (every application's playback stream,
//! with `application.name`, `media.name`, icon name and state) and their
//! ports.
//!
//! Each capture ([`LinkManager::capture`]) is a PipeWire capture stream
//! with `node.autoconnect = false` (the session manager does not link it),
//! 48 kHz stereo `f32` (`FL`, `FR`), whose process callback pushes into a
//! mixer input. The manager links, through the `link-factory`, the output
//! ports of every playback stream the capture's [`PlaybackFilter`] selects
//! to the capture's input ports (mono and centre channels into both,
//! surround channels to their side, LFE left out), and keeps the links in
//! step as streams come and go: a tab that starts playing is linked within
//! a round trip, a stream that ends takes its links with it, and a
//! restarted application is matched again by name. PipeWire sums all links
//! into an input port, so one capture stream carries any number of
//! applications. The links belong to our connection and vanish with it.
//!
//! This rather than one capture stream per application with
//! `target.object`: whether a capture may follow a playback stream is the
//! session manager's policy, which differs between WirePlumber versions,
//! while links we make ourselves need only a session manager that
//! configures stream ports (WirePlumber and media-session do, whether or
//! not a stream is linked). No extra sink or loopback module is needed, the
//! default sink and all volumes stay as they are, and our own voice
//! playback (cpal → ALSA → pipewire-alsa, a client in this process) never
//! reaches the stream.
//!
//! Our own process tree is recognised by process id: a node's
//! `application.process.id`, else its client's `application.process.id`,
//! else the client's `pipewire.sec.pid`, compared with our pid and walked up
//! the `/proc/<pid>/stat` parent chain (and, without any pid, by our
//! executable's name). In a sandbox with a pid namespace of its own the
//! daemon sees other pids than we do, so the manager's own connection
//! carries a marker property: native clients with that connection's
//! `pipewire.sec.pid` are ours too. A playback stream is linked only once
//! its node and client details have arrived, so our playback is never
//! linked, not even briefly. The playing sides of loopbacks and filters
//! (`node.link-group`) are never linked: what they replay is captured at
//! the applications that played it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::{Object, Value};
use pw::spa::utils::SpaTypes;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::capture::playback::{AudioApp, PlaybackFilter, SourceCapture, parent_pid};
use crate::capture::pw::{pod, serialize};
use crate::frame::AUDIO_SAMPLE_RATE;
use crate::mix::{SourceHandle, SourceInput};
use crate::{Error, Result};

const BACKEND: &str = "pipewire";
/// Prefix of our capture nodes' names.
pub const NODE_PREFIX: &str = "voelin-stream-audio";
/// Samples converted per push (a PipeWire quantum is at most 8192 frames).
const SCRATCH: usize = 8192 * 2;
const PLAYBACK_CLASS: &str = "Stream/Output/Audio";
/// Client property that marks the manager's own connection, whose
/// `pipewire.sec.pid` is this process as the daemon sees it (the host pid,
/// also inside a sandbox with a pid namespace).
const MARKER: &str = "voelin.link-manager";

/// `EPIPE`, what PipeWire reports when the daemon goes away.
fn libc_epipe() -> i32 {
	rustix::io::Errno::PIPE.raw_os_error()
}

fn unavailable(reason: impl Into<String>) -> Error {
	Error::CaptureUnavailable { backend: BACKEND, reason: reason.into() }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Command {
	Add {
		id: u64,
		filter: PlaybackFilter,
		name: String,
		input: SourceInput,
		reply: std_mpsc::Sender<std::result::Result<(), String>>,
	},
	Remove(u64),
	Quit,
}

/// Registry watcher and link maker on its own PipeWire connection (see the
/// module docs). One per process is enough: [`LinkManager::shared`].
pub struct LinkManager {
	commands: Mutex<pw::channel::Sender<Command>>,
	apps: watch::Receiver<Vec<AudioApp>>,
	alive: Arc<AtomicBool>,
	next_id: AtomicU64,
	thread: Option<JoinHandle<()>>,
}

static SHARED: Mutex<Option<Weak<LinkManager>>> = Mutex::new(None);

impl LinkManager {
	/// The process's manager, connected to the user's PipeWire daemon; a new
	/// one when none is alive (or the last one lost its connection).
	pub fn shared() -> Result<Arc<Self>> {
		let mut slot = lock(&SHARED);
		if let Some(manager) = slot.as_ref().and_then(Weak::upgrade)
			&& manager.is_alive()
		{
			return Ok(manager);
		}
		let manager = Arc::new(Self::connect(None)?);
		*slot = Some(Arc::downgrade(&manager));
		Ok(manager)
	}

	/// A manager of its own, connected to the daemon listening on `socket`
	/// (the path of its `pipewire-0` socket), or the user's daemon.
	pub fn connect(socket: Option<&Path>) -> Result<Self> {
		let (commands, rx) = pw::channel::channel::<Command>();
		let (apps_tx, apps) = watch::channel(Vec::new());
		let alive = Arc::new(AtomicBool::new(true));
		let (init_tx, init_rx) = std_mpsc::channel();
		let remote = socket.map(|s| s.to_string_lossy().into_owned());
		let flag = alive.clone();
		let thread = std::thread::Builder::new()
			.name("voelin-pipewire-links".into())
			.spawn(move || run(remote, rx, apps_tx, flag, init_tx))?;
		let mut manager = Self {
			commands: Mutex::new(commands),
			apps,
			alive,
			next_id: AtomicU64::new(1),
			thread: Some(thread),
		};
		match init_rx.recv() {
			Ok(Ok(())) => Ok(manager),
			Ok(Err(e)) => {
				manager.stop();
				Err(unavailable(e))
			}
			Err(_) => {
				manager.stop();
				Err(unavailable("the PipeWire thread ended during setup"))
			}
		}
	}

	/// Whether the connection is still up.
	pub fn is_alive(&self) -> bool {
		self.alive.load(Ordering::Relaxed)
	}

	/// Applications with playback streams (not ours), updated live.
	pub fn apps(&self) -> watch::Receiver<Vec<AudioApp>> {
		self.apps.clone()
	}

	/// Capture the playback `filter` selects into a new input of `source`,
	/// until the returned capture is dropped.
	pub fn capture(
		self: &Arc<Self>,
		filter: &PlaybackFilter,
		source: &SourceHandle,
	) -> Result<Box<dyn SourceCapture>> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let input = source.input(AUDIO_SAMPLE_RATE);
		let (reply, answer) = std_mpsc::channel();
		let name = source.name().to_owned();
		self.send(Command::Add { id, filter: filter.clone(), name, input, reply })?;
		match answer.recv_timeout(Duration::from_secs(5)) {
			Ok(Ok(())) => Ok(Box::new(LinkCapture { manager: self.clone(), id })),
			Ok(Err(e)) => Err(unavailable(e)),
			Err(_) => Err(unavailable("the PipeWire thread did not answer")),
		}
	}

	fn send(&self, command: Command) -> Result<()> {
		lock(&self.commands)
			.send(command)
			.map_err(|_| unavailable("the PipeWire connection is gone"))
	}

	fn stop(&mut self) {
		let _ = self.send(Command::Quit);
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

impl Drop for LinkManager {
	fn drop(&mut self) {
		self.stop();
	}
}

/// One capture of a [`LinkManager`]; removes its stream and links on drop.
struct LinkCapture {
	manager: Arc<LinkManager>,
	id: u64,
}

impl SourceCapture for LinkCapture {
	fn backend(&self) -> &'static str {
		BACKEND
	}
}

impl Drop for LinkCapture {
	fn drop(&mut self) {
		let _ = self.manager.send(Command::Remove(self.id));
	}
}

/// The manager's thread: connect, watch, link, until told to quit or the
/// daemon goes away.
fn run(
	remote: Option<String>,
	rx: pw::channel::Receiver<Command>,
	apps: watch::Sender<Vec<AudioApp>>,
	alive: Arc<AtomicBool>,
	init: std_mpsc::Sender<std::result::Result<(), String>>,
) {
	pw::init();
	static MARKERS: AtomicU64 = AtomicU64::new(1);
	let marker = format!("{}-{}", std::process::id(), MARKERS.fetch_add(1, Ordering::Relaxed));
	let setup = (|| {
		let mainloop =
			pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
		let context = pw::context::ContextRc::new(&mainloop, None)
			.map_err(|e| format!("PipeWire context: {e}"))?;
		let mut props = pw::properties::PropertiesBox::new();
		if let Some(remote) = remote {
			props.insert(*pw::keys::REMOTE_NAME, remote);
		}
		props.insert(MARKER, marker.to_string());
		let core = context
			.connect_rc(Some(props))
			.map_err(|e| format!("cannot connect to PipeWire: {e}"))?;
		let registry = core.get_registry_rc().map_err(|e| format!("PipeWire registry: {e}"))?;
		Ok::<_, String>((mainloop, context, core, registry))
	})();
	let (mainloop, _context, core, registry) = match setup {
		Ok(parts) => parts,
		Err(e) => {
			alive.store(false, Ordering::Relaxed);
			let _ = init.send(Err(e));
			return;
		}
	};
	let model = Rc::new(RefCell::new(Model::new(core.clone(), registry.clone(), apps, marker)));
	let _registry = registry
		.add_listener_local()
		.global({
			let model = Rc::downgrade(&model);
			move |global| {
				if let Some(model) = model.upgrade() {
					Model::global(&model, global);
				}
			}
		})
		.global_remove({
			let model = Rc::downgrade(&model);
			move |id| {
				if let Some(model) = model.upgrade() {
					model.borrow_mut().remove(id);
				}
			}
		})
		.register();
	let _core = core
		.add_listener_local()
		.error({
			let mainloop = mainloop.downgrade();
			let alive = alive.clone();
			move |id, _seq, res, message| {
				// Other errors (a link refused, a stale id) are for one
				// object; losing the socket ends everything.
				if id != pw::core::PW_ID_CORE || res != -libc_epipe() {
					debug!(id, res, "PipeWire error: {message}");
				} else {
					warn!("PipeWire connection lost: {message}");
					alive.store(false, Ordering::Relaxed);
					if let Some(mainloop) = mainloop.upgrade() {
						mainloop.quit();
					}
				}
			}
		})
		.register();
	let _commands = rx.attach(mainloop.loop_(), {
		let model = Rc::downgrade(&model);
		let mainloop = mainloop.downgrade();
		move |command| match command {
			Command::Quit => {
				if let Some(mainloop) = mainloop.upgrade() {
					mainloop.quit();
				}
			}
			Command::Add { id, filter, name, input, reply } => {
				let result = match model.upgrade() {
					Some(model) => model.borrow_mut().add_capture(id, filter, &name, input),
					None => Err("shutting down".into()),
				};
				let _ = reply.send(result);
			}
			Command::Remove(id) => {
				if let Some(model) = model.upgrade() {
					model.borrow_mut().remove_capture(id);
				}
			}
		}
	});
	let _ = init.send(Ok(()));
	mainloop.run();
	alive.store(false, Ordering::Relaxed);
	// Links, streams and proxies go before the connection.
	model.borrow_mut().clear();
}

/// Where a playback port goes in our stereo input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
	Left,
	Right,
	Both,
	Skip,
}

/// The side of a channel position (`audio.channel`): anything with an L is
/// left, a trailing R (or R before C, W, H) right, centres and mono both,
/// LFE nowhere; `AUX<n>` by parity.
fn side(channel: Option<&str>) -> Side {
	let Some(channel) = channel.map(str::trim) else { return Side::Both };
	let upper = channel.to_ascii_uppercase();
	let c = upper.as_str();
	if c.starts_with("LFE") {
		return Side::Skip;
	}
	if matches!(c, "MONO" | "FC" | "RC" | "TC" | "TFC" | "TRC" | "BC" | "BLC" | "BRC" | "") {
		return Side::Both;
	}
	if let Some(n) = c.strip_prefix("AUX").and_then(|n| n.parse::<u32>().ok()) {
		return if n % 2 == 0 { Side::Left } else { Side::Right };
	}
	if c.contains('L') {
		return Side::Left;
	}
	let bytes = c.as_bytes();
	let last = bytes[bytes.len() - 1];
	let before = bytes.len().checked_sub(2).map(|i| bytes[i]);
	if last == b'R' || (before == Some(b'R') && matches!(last, b'C' | b'W' | b'H')) {
		return Side::Right;
	}
	Side::Both
}

/// A process id from a property.
fn pid(props: &DictRef, key: &str) -> Option<u32> {
	props.get(key).and_then(|v| v.trim().parse().ok()).filter(|&p| p > 0)
}

fn text(props: &DictRef, key: &str) -> Option<String> {
	props.get(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

struct Client {
	/// Kernel-checked pid of the connection (`pipewire.sec.pid`); for
	/// pipewire-pulse streams that is pipewire-pulse, not the application.
	sec_pid: Option<u32>,
	pid: Option<u32>,
	binary: Option<String>,
	name: Option<String>,
	/// Its details arrived (or never will).
	known: bool,
	_proxy: Option<(pw::client::ClientListener, pw::client::Client)>,
}

struct Node {
	node_name: String,
	client: Option<u32>,
	app_name: Option<String>,
	pid: Option<u32>,
	binary: Option<String>,
	icon: Option<String>,
	media: Option<String>,
	running: bool,
	/// The playing side of a loopback or filter (`node.link-group`): it
	/// replays what applications played into a virtual sink, which is
	/// captured at those applications already.
	internal: bool,
	/// Its info arrived (or never will).
	known: bool,
	_proxy: Option<(pw::node::NodeListener, pw::node::Node)>,
}

struct Port {
	node: u32,
	output: bool,
	monitor: bool,
	channel: Option<String>,
}

/// One capture stream.
struct Capture {
	id: u64,
	filter: PlaybackFilter,
	node_name: String,
	/// Its node, once in the registry.
	node: Option<u32>,
	_listener: pw::stream::StreamListener<CaptureData>,
	_stream: pw::stream::StreamRc,
}

/// What a capture stream's callbacks own.
struct CaptureData {
	input: SourceInput,
	channels: u16,
	scratch: Vec<f32>,
}

impl CaptureData {
	fn process(&mut self, stream: &pw::stream::Stream) {
		let Some(mut buffer) = stream.dequeue_buffer() else { return };
		let Some(data) = buffer.datas_mut().first_mut() else { return };
		let chunk = data.chunk();
		let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
		let Some(bytes) = data.data() else { return };
		let Some(bytes) = bytes.get(offset..offset + size) else { return };
		let channels = usize::from(self.channels.max(1));
		let part = SCRATCH / channels * channels * 4;
		for part in bytes.chunks(part) {
			self.scratch.clear();
			self.scratch
				.extend(part.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
			self.input.push(&self.scratch, self.channels);
		}
	}
}

/// The registry as far as it matters here, our captures and our links.
struct Model {
	core: pw::core::CoreRc,
	registry: pw::registry::RegistryRc,
	own_pid: u32,
	/// Our pid as the daemon sees it (see [`MARKER`]).
	own_sec_pid: Option<u32>,
	marker: String,
	own_binary: Option<String>,
	clients: HashMap<u32, Client>,
	nodes: HashMap<u32, Node>,
	ports: HashMap<u32, Port>,
	captures: Vec<Capture>,
	/// Links we made, by (output port, input port).
	links: HashMap<(u32, u32), pw::link::Link>,
	/// Parent chains of process ids, cached until a client goes.
	chains: RefCell<HashMap<u32, Vec<u32>>>,
	apps: watch::Sender<Vec<AudioApp>>,
	weak: RcWeak<RefCell<Model>>,
}

impl Model {
	fn new(
		core: pw::core::CoreRc,
		registry: pw::registry::RegistryRc,
		apps: watch::Sender<Vec<AudioApp>>,
		marker: String,
	) -> Self {
		let own_binary = std::env::current_exe()
			.ok()
			.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
		Self {
			core,
			registry,
			own_pid: std::process::id(),
			own_sec_pid: None,
			marker,
			own_binary,
			clients: HashMap::new(),
			nodes: HashMap::new(),
			ports: HashMap::new(),
			captures: Vec::new(),
			links: HashMap::new(),
			chains: RefCell::new(HashMap::new()),
			apps,
			weak: RcWeak::new(),
		}
	}

	fn global(model: &Rc<RefCell<Model>>, global: &pw::registry::GlobalObject<&DictRef>) {
		let Some(props) = global.props else { return };
		let mut this = model.borrow_mut();
		if this.weak.upgrade().is_none() {
			this.weak = Rc::downgrade(model);
		}
		let id = global.id;
		match global.type_ {
			ObjectType::Client => {
				if props.get(MARKER) == Some(this.marker.as_str()) {
					this.own_sec_pid = pid(props, "pipewire.sec.pid");
				}
				let proxy =
					this.registry.bind::<pw::client::Client, _>(global).ok().map(|client| {
						let model = Rc::downgrade(model);
						let listener = client
							.add_listener_local()
							.info(move |info| {
								if let Some(model) = model.upgrade() {
									model.borrow_mut().client_info(id, info.props());
								}
							})
							.register();
						(listener, client)
					});
				let client = Client {
					sec_pid: pid(props, "pipewire.sec.pid"),
					pid: pid(props, "application.process.id"),
					binary: text(props, "application.process.binary"),
					name: text(props, "application.name"),
					known: proxy.is_none(),
					_proxy: proxy,
				};
				this.clients.insert(id, client);
			}
			ObjectType::Node => {
				let node_name = props.get("node.name").unwrap_or_default().to_owned();
				if node_name.starts_with(NODE_PREFIX) {
					let mut ours = false;
					for capture in this.captures.iter_mut().filter(|c| c.node_name == node_name) {
						capture.node = Some(id);
						ours = true;
					}
					if ours {
						debug!(node = id, %node_name, "capture node ready");
						this.reconcile();
					}
					return;
				}
				if props.get("media.class") != Some(PLAYBACK_CLASS) {
					return;
				}
				let proxy = this.registry.bind::<pw::node::Node, _>(global).ok().map(|node| {
					let model = Rc::downgrade(model);
					let listener = node
						.add_listener_local()
						.info(move |info| {
							if let Some(model) = model.upgrade() {
								let running = matches!(info.state(), pw::node::NodeState::Running);
								model.borrow_mut().node_info(id, running, info.props());
							}
						})
						.register();
					(listener, node)
				});
				let node = Node {
					node_name,
					client: pid(props, "client.id"),
					app_name: text(props, "application.name"),
					pid: pid(props, "application.process.id"),
					binary: text(props, "application.process.binary"),
					icon: None,
					media: text(props, "media.name"),
					running: false,
					internal: props.get("node.link-group").is_some(),
					known: proxy.is_none(),
					_proxy: proxy,
				};
				this.nodes.insert(id, node);
				this.changed();
			}
			ObjectType::Port => {
				let Some(node) = pid(props, "node.id") else { return };
				let port = Port {
					node,
					output: props.get("port.direction") == Some("out"),
					monitor: props.get("port.monitor") == Some("true"),
					channel: text(props, "audio.channel"),
				};
				this.ports.insert(id, port);
				let relevant = this.nodes.contains_key(&node)
					|| this.captures.iter().any(|c| c.node == Some(node));
				if relevant {
					this.reconcile();
				}
			}
			_ => {}
		}
	}

	fn client_info(&mut self, id: u32, props: Option<&DictRef>) {
		let Some(client) = self.clients.get_mut(&id) else { return };
		if let Some(props) = props {
			client.pid = pid(props, "application.process.id").or(client.pid);
			client.sec_pid = pid(props, "pipewire.sec.pid").or(client.sec_pid);
			client.binary = text(props, "application.process.binary").or(client.binary.take());
			client.name = text(props, "application.name").or(client.name.take());
		}
		client.known = true;
		self.changed();
	}

	fn node_info(&mut self, id: u32, running: bool, props: Option<&DictRef>) {
		let Some(node) = self.nodes.get_mut(&id) else { return };
		node.running = running;
		if let Some(props) = props {
			node.app_name = text(props, "application.name").or(node.app_name.take());
			node.pid = pid(props, "application.process.id").or(node.pid);
			node.binary = text(props, "application.process.binary").or(node.binary.take());
			node.icon = text(props, "application.icon-name")
				.or_else(|| text(props, "application.icon_name"))
				.or_else(|| text(props, "media.icon-name"))
				.or(node.icon.take());
			node.media = text(props, "media.name").or(node.media.take());
			node.client = pid(props, "client.id").or(node.client);
			node.internal |= props.get("node.link-group").is_some();
		}
		node.known = true;
		self.changed();
	}

	fn remove(&mut self, id: u32) {
		if self.clients.remove(&id).is_some() {
			// Its process may be gone and its id reused.
			self.chains.borrow_mut().clear();
		}
		let node = self.nodes.remove(&id).is_some();
		let port = self.ports.remove(&id).is_some();
		let mut capture = false;
		for c in self.captures.iter_mut().filter(|c| c.node == Some(id)) {
			c.node = None;
			capture = true;
		}
		if port {
			self.links.retain(|(out, input), _| *out != id && *input != id);
		}
		if node || capture {
			self.changed();
		} else if port {
			self.reconcile();
		}
	}

	fn add_capture(
		&mut self,
		id: u64,
		filter: PlaybackFilter,
		name: &str,
		input: SourceInput,
	) -> std::result::Result<(), String> {
		let node_name = format!("{NODE_PREFIX}-{}-{id}", self.own_pid);
		let description = format!("Voelin stream audio: {}", name.replace('\0', ""));
		let mut props = pw::properties::PropertiesBox::new();
		props.insert(*pw::keys::MEDIA_TYPE, "Audio");
		props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
		props.insert(*pw::keys::MEDIA_ROLE, "Music");
		props.insert(*pw::keys::NODE_NAME, node_name.as_str());
		props.insert(*pw::keys::NODE_DESCRIPTION, description);
		props.insert("node.autoconnect", "false");
		let stream = pw::stream::StreamRc::new(self.core.clone(), &node_name, props)
			.map_err(|e| format!("PipeWire stream: {e}"))?;
		let data = CaptureData { input, channels: 2, scratch: Vec::with_capacity(SCRATCH) };
		let listener = stream
			.add_local_listener_with_user_data(data)
			.state_changed(|_, _, _, new| {
				if let pw::stream::StreamState::Error(e) = &new {
					warn!("application audio stream failed: {e}");
				}
			})
			.param_changed(|_, data, id, param| {
				let Some(param) = param else { return };
				if id != pw::spa::param::ParamType::Format.as_raw() {
					return;
				}
				let mut info = AudioInfoRaw::new();
				if info.parse(param).is_ok() && info.channels() > 0 {
					data.channels = info.channels().min(u32::from(u16::MAX)) as u16;
				}
			})
			.process(|stream, data| data.process(stream))
			.register()
			.map_err(|e| format!("PipeWire listener: {e}"))?;
		let mut info = AudioInfoRaw::new();
		info.set_format(AudioFormat::F32LE);
		info.set_rate(AUDIO_SAMPLE_RATE);
		info.set_channels(2);
		let mut position = [0; pw::spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
		position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
		position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
		info.set_position(position);
		let format = serialize(Value::Object(Object {
			type_: SpaTypes::ObjectParamFormat.as_raw(),
			id: pw::spa::param::ParamType::EnumFormat.as_raw(),
			properties: info.into(),
		}))?;
		stream
			.connect(
				pw::spa::utils::Direction::Input,
				None,
				pw::stream::StreamFlags::MAP_BUFFERS,
				&mut [pod(&format)?],
			)
			.map_err(|e| format!("cannot connect the application audio stream: {e}"))?;
		debug!(id, ?filter, %node_name, "application audio capture");
		self.captures.push(Capture {
			id,
			filter,
			node_name,
			node: None,
			_listener: listener,
			_stream: stream,
		});
		Ok(())
	}

	fn remove_capture(&mut self, id: u64) {
		self.captures.retain(|c| c.id != id);
		self.reconcile();
	}

	fn clear(&mut self) {
		self.links.clear();
		self.captures.clear();
		self.nodes.clear();
		self.clients.clear();
		self.ports.clear();
	}

	/// The parent chain of `pid`, itself first.
	fn chain(&self, pid: u32) -> Vec<u32> {
		if let Some(chain) = self.chains.borrow().get(&pid) {
			return chain.clone();
		}
		let mut chain = vec![pid];
		let mut current = pid;
		while chain.len() < 64 {
			match parent_pid(current) {
				Some(parent) if parent > 0 && parent != current => {
					chain.push(parent);
					current = parent;
				}
				_ => break,
			}
		}
		self.chains.borrow_mut().insert(pid, chain.clone());
		chain
	}

	/// The process of a node: its own `application.process.id`, else its
	/// client's, else the client's socket credentials.
	fn node_pid(&self, node: &Node) -> Option<u32> {
		let client = node.client.and_then(|c| self.clients.get(&c));
		node.pid.or(client.and_then(|c| c.pid)).or(client.and_then(|c| c.sec_pid))
	}

	/// The node's and its client's details are in.
	fn resolved(&self, node: &Node) -> bool {
		node.known && node.client.and_then(|c| self.clients.get(&c)).is_none_or(|c| c.known)
	}

	/// Played by this process or one of its children.
	fn is_ours(&self, node: &Node) -> bool {
		let client = node.client.and_then(|c| self.clients.get(&c));
		// A native client of ours, as the daemon saw its socket.
		if node.pid.is_none()
			&& self.own_sec_pid.is_some()
			&& client.and_then(|c| c.sec_pid) == self.own_sec_pid
		{
			return true;
		}
		match self.node_pid(node) {
			Some(pid) => self.chain(pid).contains(&self.own_pid),
			None => {
				let binary = node.binary.as_deref().or(client.and_then(|c| c.binary.as_deref()));
				binary.is_some() && binary == self.own_binary.as_deref()
			}
		}
	}

	fn matches(&self, filter: &PlaybackFilter, node: &Node) -> bool {
		if node.internal || self.is_ours(node) {
			return false;
		}
		match filter {
			PlaybackFilter::AllButSelf => true,
			PlaybackFilter::App(app @ crate::capture::playback::AppMatch::Name(_)) => {
				let client = node.client.and_then(|c| self.clients.get(&c));
				let names = [
					node.app_name.as_deref(),
					node.binary.as_deref(),
					client.and_then(|c| c.name.as_deref()),
					client.and_then(|c| c.binary.as_deref()),
					Some(node.node_name.as_str()),
				];
				app.matches_name(names.into_iter().flatten())
			}
			PlaybackFilter::App(crate::capture::playback::AppMatch::Pid(wanted)) => {
				let client = node.client.and_then(|c| self.clients.get(&c));
				[node.pid, client.and_then(|c| c.pid), client.and_then(|c| c.sec_pid)]
					.into_iter()
					.flatten()
					.any(|pid| self.chain(pid).contains(wanted))
			}
		}
	}

	/// Nodes or clients changed: links and the app list follow.
	fn changed(&mut self) {
		self.reconcile();
		self.publish();
	}

	/// Make the links match what every capture selects now.
	fn reconcile(&mut self) {
		let mut wanted: HashMap<(u32, u32), (u32, u32)> = HashMap::new();
		for capture in &self.captures {
			let Some(ours) = capture.node else { continue };
			let mut inputs = (None, None);
			for (&id, port) in &self.ports {
				if port.node != ours || port.output || port.monitor {
					continue;
				}
				match side(port.channel.as_deref()) {
					Side::Left => inputs.0 = Some(id),
					Side::Right => inputs.1 = Some(id),
					_ => {}
				}
			}
			if inputs == (None, None) {
				// Not configured by the session manager yet.
				continue;
			}
			for (&node_id, node) in &self.nodes {
				if !self.resolved(node) || !self.matches(&capture.filter, node) {
					continue;
				}
				let outputs: Vec<(u32, &Port)> = self
					.ports
					.iter()
					.filter(|(_, p)| p.node == node_id && p.output && !p.monitor)
					.map(|(&id, p)| (id, p))
					.collect();
				let single = outputs.len() == 1;
				for (port_id, port) in outputs {
					let side = if single { Side::Both } else { side(port.channel.as_deref()) };
					let targets = match side {
						Side::Left => [inputs.0, None],
						Side::Right => [inputs.1, None],
						Side::Both => [inputs.0, inputs.1],
						Side::Skip => [None, None],
					};
					for input in targets.into_iter().flatten() {
						wanted.insert((port_id, input), (node_id, ours));
					}
				}
			}
		}
		self.links.retain(|pair, _| wanted.contains_key(pair));
		for (pair, nodes) in wanted {
			if self.links.contains_key(&pair) {
				continue;
			}
			let mut props = pw::properties::PropertiesBox::new();
			props.insert("link.output.node", nodes.0.to_string());
			props.insert("link.output.port", pair.0.to_string());
			props.insert("link.input.node", nodes.1.to_string());
			props.insert("link.input.port", pair.1.to_string());
			props.insert("object.linger", "false");
			match self.core.create_object::<pw::link::Link>("link-factory", &props) {
				Ok(link) => {
					debug!(output = pair.0, input = pair.1, "linked");
					self.links.insert(pair, link);
				}
				Err(e) => warn!("cannot link port {} to {}: {e}", pair.0, pair.1),
			}
		}
	}

	/// Send the list of playing applications if it changed.
	fn publish(&self) {
		let mut apps: Vec<AudioApp> = Vec::new();
		for node in self.nodes.values() {
			if !self.resolved(node) || node.internal || self.is_ours(node) {
				continue;
			}
			let client = node.client.and_then(|c| self.clients.get(&c));
			let pid = self.node_pid(node);
			let binary = node.binary.clone().or_else(|| client.and_then(|c| c.binary.clone()));
			let name = node
				.app_name
				.clone()
				.or_else(|| client.and_then(|c| c.name.clone()))
				.or_else(|| binary.clone())
				.unwrap_or_else(|| node.node_name.clone());
			let existing = apps.iter_mut().find(|a| match pid {
				Some(pid) => a.pid == Some(pid),
				None => a.pid.is_none() && a.name == name,
			});
			match existing {
				Some(app) => {
					app.streams += 1;
					app.playing |= node.running;
					if app.media.is_none() || (node.running && !app.playing) {
						app.media = node.media.clone();
					}
					if app.icon.is_none() {
						app.icon = node.icon.clone();
					}
				}
				None => apps.push(AudioApp {
					name,
					pid,
					binary,
					icon: node.icon.clone(),
					media: node.media.clone(),
					streams: 1,
					playing: node.running,
				}),
			}
		}
		apps.sort_by(|a, b| {
			a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.pid.cmp(&b.pid))
		});
		self.apps.send_if_modified(|current| {
			if *current == apps {
				false
			} else {
				*current = apps;
				true
			}
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn channel_sides() {
		for (channel, expected) in [
			("FL", Side::Left),
			("FR", Side::Right),
			("MONO", Side::Both),
			("FC", Side::Both),
			("LFE", Side::Skip),
			("RL", Side::Left),
			("RR", Side::Right),
			("SL", Side::Left),
			("SR", Side::Right),
			("FLC", Side::Left),
			("FRC", Side::Right),
			("RC", Side::Both),
			("TFL", Side::Left),
			("TRR", Side::Right),
			("FRW", Side::Right),
			("AUX0", Side::Left),
			("AUX1", Side::Right),
			("UNK", Side::Both),
		] {
			assert_eq!(side(Some(channel)), expected, "{channel}");
		}
		assert_eq!(side(None), Side::Both);
	}

	/// Without a daemon, connecting fails with a clear error instead of
	/// hanging. Skipped where a daemon socket exists.
	#[test]
	fn unavailable_without_daemon() {
		let err = LinkManager::connect(Some(Path::new("/nonexistent/voelin/pipewire-0")))
			.err()
			.expect("no daemon there");
		assert!(matches!(err, Error::CaptureUnavailable { backend: "pipewire", .. }), "{err}");
	}
}
