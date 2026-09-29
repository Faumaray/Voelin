//! `voelinctl engine`: a voice session through the client engine
//! (voelin-core), for what the app does with a voice connection: files,
//! avatars and icons, pokes, private and offline messages, contacts.
//!
//! Prints one line per interesting event (`files ...`, `transfer ...`,
//! `poke ...`, `chat ...`, `avatar ...`, `icon ...`, `offline ...`,
//! `friend ...`, `contacts ...`, `done ...`, `error: ...`). The action
//! decides when the run succeeded; `--seconds` bounds it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout_at};
use voelin_core::settings::Settings;
use voelin_core::{
	Command, Contact, DownloadTo, Engine, Event, History, Relation, TransferState, VoiceOptions,
	VoiceState,
};
use voelin_model::{ChatTarget, Presence};

use crate::gateway::target_name;

const SESSION: u64 = 1;

#[derive(Args, Debug)]
pub struct EngineArgs {
	/// Server address (host:port).
	pub address: String,
	/// Identity file (default: a new identity).
	#[arg(long, env = "VOELINCTL_IDENTITY")]
	pub identity: Option<PathBuf>,
	#[arg(long, default_value = "voelinctl")]
	pub nick: String,
	/// Client database: chat history and contacts (default: in memory).
	#[arg(long)]
	pub db: Option<PathBuf>,
	/// Avatar and icon cache directory (default: a new temporary one).
	#[arg(long)]
	pub cache: Option<PathBuf>,
	/// Give up after this many seconds.
	#[arg(long, default_value_t = 30)]
	pub seconds: u64,
	/// An engine setting, e.g. `privacy.block_mode=flag`. Repeatable.
	#[arg(long = "set", value_name = "KEY=VALUE")]
	pub set: Vec<String>,
	#[command(subcommand)]
	pub action: EngineAction,
}

#[derive(Subcommand, Debug)]
pub enum EngineAction {
	/// A channel's file browser.
	Files {
		#[command(subcommand)]
		command: FilesCommand,
	},
	/// Our avatar, or wait for someone else's.
	Avatar {
		#[command(subcommand)]
		command: AvatarCommand,
	},
	/// Poke the client with this nickname.
	Poke {
		#[arg(long)]
		to: String,
		#[arg(long)]
		message: String,
	},
	/// Send a private message to the client with this nickname.
	Dm {
		#[arg(long)]
		to: String,
		#[arg(long)]
		message: String,
	},
	/// Offline messages.
	Offline {
		#[command(subcommand)]
		command: OfflineCommand,
	},
	/// Contacts in `--db`.
	Contacts {
		#[command(subcommand)]
		command: ContactsCommand,
	},
	/// Print events; with `--expect`, succeed once a line contains it.
	Listen {
		#[arg(long)]
		expect: Option<String>,
		/// Download the first file linked in chat into this directory
		/// (succeeds when it is there).
		#[arg(long)]
		fetch_links: Option<PathBuf>,
	},
}

#[derive(Subcommand, Debug)]
pub enum FilesCommand {
	/// List a directory.
	Ls {
		#[arg(long, default_value_t = 1)]
		channel: u64,
		#[arg(long, default_value = "/")]
		path: String,
		/// Succeed only if an entry of this name is listed.
		#[arg(long)]
		expect: Option<String>,
	},
	/// Upload a local file.
	Upload {
		file: PathBuf,
		#[arg(long, default_value_t = 1)]
		channel: u64,
		/// Remote path (default: `/<file name>`).
		#[arg(long)]
		path: Option<String>,
		#[arg(long)]
		overwrite: bool,
		/// Then post a link to it in our channel's chat.
		#[arg(long)]
		share: bool,
	},
	/// Download a file.
	Download {
		#[arg(long, default_value_t = 1)]
		channel: u64,
		#[arg(long)]
		path: String,
		#[arg(long)]
		out: PathBuf,
		#[arg(long)]
		resume: bool,
	},
	/// Delete files or directories.
	Rm {
		#[arg(long, default_value_t = 1)]
		channel: u64,
		paths: Vec<String>,
	},
	Mkdir {
		#[arg(long, default_value_t = 1)]
		channel: u64,
		path: String,
	},
	Mv {
		#[arg(long, default_value_t = 1)]
		channel: u64,
		from: String,
		to: String,
	},
}

#[derive(Subcommand, Debug)]
pub enum AvatarCommand {
	/// Upload an image as our avatar.
	Set { image: PathBuf },
	/// Remove our avatar.
	Clear,
	/// Wait until the avatar of the client with this nickname is fetched.
	Wait {
		#[arg(long)]
		nick: String,
		/// Fail unless the file's MD5 is this.
		#[arg(long)]
		md5: Option<String>,
	},
}

#[derive(Subcommand, Debug)]
pub enum OfflineCommand {
	/// Send to a unique id.
	Send {
		#[arg(long)]
		to_uid: String,
		#[arg(long)]
		subject: String,
		#[arg(long)]
		message: String,
	},
	/// List the inbox and read every message; with `--expect`, succeed if a
	/// text contains it; with `--delete`, delete what was read.
	Read {
		#[arg(long)]
		expect: Option<String>,
		#[arg(long)]
		delete: bool,
	},
}

#[derive(Subcommand, Debug)]
pub enum ContactsCommand {
	/// Add or change a contact.
	Set {
		uid: String,
		#[arg(long, value_parser = ["friend", "blocked", "neutral"], default_value = "neutral")]
		relation: String,
		#[arg(long, default_value = "")]
		nickname: String,
		#[arg(long, default_value = "")]
		note: String,
	},
	Rm {
		uid: String,
	},
	/// Print the contacts, and friends' presence while connected.
	Ls,
}

/// What the action waits for.
#[derive(Default)]
struct Wait {
	/// Answers still missing, by request or transfer id.
	requests: usize,
	done: bool,
}

pub async fn run(args: EngineArgs) -> Result<()> {
	let settings = Settings::in_memory();
	if let Some(e) = settings.apply_overrides(args.set.iter().map(String::as_str)).first() {
		bail!("--set: {e}");
	}
	let history = match &args.db {
		Some(path) => {
			History::open(path).with_context(|| format!("database {}", path.display()))?
		}
		None => History::in_memory(),
	};
	let engine = Engine::start_with(settings, history);
	let mut events = engine.subscribe();
	let temp_cache = args.cache.is_none();
	let cache = args.cache.clone().unwrap_or_else(|| {
		std::env::temp_dir().join(format!("voelinctl-cache-{}", std::process::id()))
	});
	engine.send(Command::AttachCache(cache.clone()));
	let deadline = Instant::now() + Duration::from_secs(args.seconds);

	// Contacts need no server.
	if let EngineAction::Contacts { command } = &args.action
		&& !matches!(command, ContactsCommand::Ls)
	{
		let result = contacts(&engine, command, &mut events, deadline).await;
		engine.history().flush();
		return result;
	}

	let mut options = VoiceOptions::new(&args.address, &args.nick);
	if let Some(path) = &args.identity {
		options.identity = Some(crate::identity::load(path)?);
	}
	engine.send(Command::ConnectVoice { session: SESSION, options: Box::new(options) });

	let mut run = Runner {
		engine: engine.clone(),
		args: &args,
		presence: Arc::new(Presence::default()),
		server_uid: None,
		connected: false,
		started: false,
		wait: Wait::default(),
		read: BTreeMap::new(),
		matched: false,
		fetched_link: false,
		finish_at: None,
	};
	let result = loop {
		let wake = run.finish_at.map_or(deadline, |f| f.min(deadline));
		let event = match timeout_at(wake, events.recv()).await {
			Ok(Ok(e)) => e,
			Ok(Err(RecvError::Lagged(_))) => continue,
			Ok(Err(RecvError::Closed)) => break Err(anyhow::anyhow!("engine stopped")),
			Err(_) if run.finish_at.is_some_and(|f| Instant::now() >= f) => break Ok(()),
			Err(_) => break Err(anyhow::anyhow!("timed out")),
		};
		match run.event(event) {
			Ok(true) => break Ok(()),
			Ok(false) => {}
			Err(e) => break Err(e),
		}
	};
	engine.send(Command::DisconnectVoice { session: SESSION });
	engine.history().flush();
	// Let the disconnect go out.
	tokio::time::sleep(Duration::from_millis(300)).await;
	if temp_cache {
		let _ = std::fs::remove_dir_all(&cache);
	}
	result
}

struct Runner<'a> {
	engine: Engine,
	args: &'a EngineArgs,
	presence: Arc<Presence>,
	server_uid: Option<String>,
	connected: bool,
	started: bool,
	wait: Wait,
	/// Offline messages being read: id → subject.
	read: BTreeMap<u32, String>,
	/// An offline message had the expected text.
	matched: bool,
	fetched_link: bool,
	/// Succeed at this time unless an error comes first (commands that
	/// have no answer on success).
	finish_at: Option<Instant>,
}

impl Runner<'_> {
	fn send(&self, command: Command) {
		self.engine.send(command);
	}

	fn client_id(&self, nick: &str) -> Result<u16> {
		self.presence
			.clients
			.values()
			.find(|c| c.nickname == nick)
			.map(|c| c.id)
			.with_context(|| format!("no client {nick:?} on the server"))
	}

	/// Handle one event; `Ok(true)`: the action succeeded.
	fn event(&mut self, event: Event) -> Result<bool> {
		match event {
			Event::State { session: SESSION, state } => {
				if state.voice == VoiceState::Connected {
					self.connected = true;
				}
				if state.server_uid.is_some() {
					self.server_uid = state.server_uid;
				}
			}
			Event::Presence { session: SESSION, presence } => {
				self.presence = presence;
				if self.connected && !self.started && !self.presence.clients.is_empty() {
					self.started = true;
					println!("connected: {} clients", self.presence.clients.len());
					return self.start();
				}
			}
			Event::Error { session: SESSION, message } => {
				println!("error: {message}");
				if !matches!(self.args.action, EngineAction::Listen { .. }) {
					bail!("{message}");
				}
			}
			Event::FileList { request, channel, path, result, .. } => match result {
				Ok(entries) => {
					for e in &entries {
						let kind = if e.is_dir { "dir" } else { "file" };
						println!("files {channel}:{path} {kind} {} {} bytes", e.name, e.size);
					}
					println!("files {channel}:{path} n={} (request {request})", entries.len());
					if let EngineAction::Files { command: FilesCommand::Ls { expect, .. } } =
						&self.args.action
					{
						if let Some(name) = expect
							&& !entries.iter().any(|e| &e.name == name)
						{
							bail!("{name} is not listed");
						}
						return Ok(true);
					}
				}
				Err(e) => bail!("listing {channel}:{path}: {e}"),
			},
			Event::Transfer { transfer, state, .. } => {
				match &state {
					TransferState::Progress { .. } => {}
					TransferState::Done { size, path, .. } => {
						let path = path.as_ref().map(|p| p.display().to_string());
						println!(
							"transfer {transfer} done {size} bytes {}",
							path.unwrap_or_default()
						);
					}
					state => println!("transfer {transfer} {state:?}"),
				}
				match state {
					TransferState::Failed(e) => bail!("transfer {transfer}: {e}"),
					TransferState::Cancelled => bail!("transfer {transfer} cancelled"),
					TransferState::Done { size, .. } => return self.transfer_done(size),
					_ => {}
				}
			}
			Event::RequestDone { request, result, .. } => {
				match &result {
					Ok(()) => println!("done {request}"),
					Err(e) => println!("done {request} failed: {e}"),
				}
				result.map_err(|e| anyhow::anyhow!("request {request}: {e}"))?;
				self.wait.requests = self.wait.requests.saturating_sub(1);
				if self.wait.requests == 0 && self.wait.done {
					return Ok(true);
				}
			}
			Event::Poke { from_name, message, blocked, .. } => {
				println!(
					"poke from {from_name}: {message}{}",
					if blocked { " (blocked)" } else { "" }
				);
				return Ok(self.expected(&message));
			}
			Event::Chat { session: SESSION, message } => {
				println!(
					"chat {} {}{}: {}",
					target_name(&message.target),
					message.author_name,
					if message.blocked { " (blocked)" } else { "" },
					message.text
				);
				for file in message.file_refs() {
					println!(
						"file link {}:{} ({:?} bytes, server {:?})",
						file.channel,
						file.full_path(),
						file.size,
						file.server_uid
					);
					if let EngineAction::Listen { fetch_links: Some(dir), .. } = &self.args.action
						&& !self.fetched_link
					{
						self.fetched_link = true;
						let to = DownloadTo::Path { path: dir.join(&file.name), resume: false };
						self.send(Command::DownloadChatFile {
							session: SESSION,
							transfer: 7,
							file,
							password: None,
							to,
						});
					}
				}
				let private = matches!(message.target, ChatTarget::Private(_));
				// Our own message comes back once the server delivered it.
				if let EngineAction::Dm { message: sent, .. } = &self.args.action {
					return Ok(private && message.text == *sent);
				}
				return Ok(private && self.expected(&message.text));
			}
			Event::AvatarReady { client_uid, path, hash, .. } => {
				let nick = self
					.presence
					.client_by_uid(&client_uid)
					.map_or("?", |c| c.nickname.as_str())
					.to_owned();
				println!("avatar {nick} {client_uid} {hash} {}", path.display());
				if let EngineAction::Avatar { command: AvatarCommand::Wait { nick: want, md5 } } =
					&self.args.action
					&& *want == nick
				{
					let data = std::fs::read(&path)?;
					let actual = format!("{:x}", md5_of(&data));
					if actual != hash {
						bail!("avatar file hash {actual} is not the announced {hash}");
					}
					if let Some(md5) = md5
						&& *md5 != actual
					{
						bail!("avatar hash {actual}, expected {md5}");
					}
					return Ok(true);
				}
			}
			Event::IconReady { icon, path, .. } => println!("icon {icon} {}", path.display()),
			Event::OfflineMessages { result, .. } => {
				let list = result.map_err(|e| anyhow::anyhow!("messagelist: {e}"))?;
				println!("offline inbox n={}", list.len());
				if list.is_empty() {
					bail!("no offline messages");
				}
				for (i, m) in list.iter().enumerate() {
					println!(
						"offline {} from {} {:?} read={}",
						m.id, m.from_uid, m.subject, m.read
					);
					self.read.insert(m.id, m.subject.clone());
					self.send(Command::GetOfflineMessage {
						session: SESSION,
						request: 100 + i as u64,
						id: m.id,
					});
				}
			}
			Event::OfflineMessage { result, .. } => {
				let m = result.map_err(|e| anyhow::anyhow!("messageget: {e}"))?;
				println!("offline {} from {} {:?}: {}", m.id, m.from_uid, m.subject, m.text);
				self.read.remove(&m.id);
				self.matched |= self.expected(&m.text);
				if let EngineAction::Offline {
					command: OfflineCommand::Read { delete: true, .. },
				} = &self.args.action
				{
					self.wait.requests += 1;
					let request = 1000 + u64::from(m.id);
					self.send(Command::DeleteOfflineMessage {
						session: SESSION,
						request,
						id: m.id,
					});
				}
				if self.read.is_empty() {
					let expecting = matches!(
						&self.args.action,
						EngineAction::Offline {
							command: OfflineCommand::Read { expect: Some(_), .. }
						}
					);
					if expecting && !self.matched {
						bail!("no offline message contains the expected text");
					}
					self.wait.done = true;
					return Ok(self.wait.requests == 0);
				}
			}
			Event::FriendPresence { uid, sessions } => {
				let spots: Vec<String> = sessions
					.iter()
					.map(|s| format!("{} as {} in {}", s.server_name, s.nickname, s.channel_name))
					.collect();
				println!("friend {uid}: [{}]", spots.join(", "));
			}
			Event::ServerDetails { details, .. } => {
				println!(
					"server {:?} {} {} icon={} banner={:?}",
					details.name,
					details.platform,
					details.version,
					details.icon,
					details.banner_gfx_url
				);
			}
			Event::Groups { server_groups, channel_groups, .. } => {
				println!(
					"groups: {} server, {} channel",
					server_groups.len(),
					channel_groups.len()
				);
			}
			_ => {}
		}
		Ok(false)
	}

	/// `--expect` of listen / offline read.
	fn expected(&self, text: &str) -> bool {
		let expect = match &self.args.action {
			EngineAction::Listen { expect, .. } => expect,
			EngineAction::Offline { command: OfflineCommand::Read { expect, .. } } => expect,
			_ => return false,
		};
		expect.as_ref().is_some_and(|e| text.contains(e.as_str()))
	}

	/// Voice is up: do the action.
	fn start(&mut self) -> Result<bool> {
		let session = SESSION;
		match &self.args.action {
			EngineAction::Files { command } => match command {
				FilesCommand::Ls { channel, path, .. } => self.send(Command::ListFiles {
					session,
					request: 1,
					channel: *channel,
					password: None,
					path: path.clone(),
				}),
				FilesCommand::Upload { file, channel, path, overwrite, .. } => {
					let name = file.file_name().context("file name")?.to_string_lossy();
					let path = path.clone().unwrap_or_else(|| format!("/{name}"));
					self.send(Command::UploadFile {
						session,
						transfer: 1,
						channel: *channel,
						password: None,
						path,
						from: file.clone(),
						overwrite: *overwrite,
						resume: false,
					});
				}
				FilesCommand::Download { channel, path, out, resume } => {
					self.send(Command::DownloadFile {
						session,
						transfer: 1,
						channel: *channel,
						password: None,
						path: path.clone(),
						to: DownloadTo::Path { path: out.clone(), resume: *resume },
					});
				}
				FilesCommand::Rm { channel, paths } => {
					self.wait = Wait { requests: 1, done: true };
					self.send(Command::DeleteFiles {
						session,
						request: 1,
						channel: *channel,
						password: None,
						paths: paths.clone(),
					});
				}
				FilesCommand::Mkdir { channel, path } => {
					self.wait = Wait { requests: 1, done: true };
					self.send(Command::CreateDirectory {
						session,
						request: 1,
						channel: *channel,
						password: None,
						path: path.clone(),
					});
				}
				FilesCommand::Mv { channel, from, to } => {
					self.wait = Wait { requests: 1, done: true };
					self.send(Command::RenameFile {
						session,
						request: 1,
						channel: *channel,
						password: None,
						from: from.clone(),
						to: to.clone(),
						to_channel: None,
					});
				}
			},
			EngineAction::Avatar { command } => match command {
				AvatarCommand::Set { image } => {
					self.wait = Wait { requests: 1, done: true };
					let image = Some(image.clone());
					self.send(Command::SetAvatar { session, request: 1, image });
				}
				AvatarCommand::Clear => {
					self.wait = Wait { requests: 1, done: true };
					self.send(Command::SetAvatar { session, request: 1, image: None });
				}
				AvatarCommand::Wait { nick, .. } => {
					// Also when it was fetched before this run looked.
					if let Some(uid) = self
						.presence
						.clients
						.values()
						.find(|c| c.nickname == *nick)
						.and_then(|c| c.uid.clone())
					{
						self.send(Command::FetchAvatar { session, client_uid: uid });
					}
				}
			},
			EngineAction::Poke { to, message } => {
				let client = self.client_id(to)?;
				self.send(Command::Poke { session, client, message: message.clone() });
				// No answer on success: an error would come quickly.
				self.finish_at = Some(Instant::now() + Duration::from_secs(1));
			}
			EngineAction::Dm { to, message } => {
				let uid = self
					.presence
					.clients
					.values()
					.find(|c| c.nickname == *to)
					.and_then(|c| c.uid.clone())
					.with_context(|| format!("no client {to:?} with a unique id"))?;
				let target = ChatTarget::Private(uid);
				self.send(Command::OpenChat { session, target: target.clone() });
				self.send(Command::SendChat { session, target, text: message.clone() });
			}
			EngineAction::Offline { command } => match command {
				OfflineCommand::Send { to_uid, subject, message } => {
					self.wait = Wait { requests: 1, done: true };
					self.send(Command::SendOfflineMessage {
						session,
						request: 1,
						to_uid: to_uid.clone(),
						subject: subject.clone(),
						text: message.clone(),
					});
				}
				OfflineCommand::Read { .. } => {
					self.send(Command::ListOfflineMessages { session, request: 1 });
				}
			},
			EngineAction::Contacts { .. } => {
				for c in self.engine.contacts() {
					println!("contact {} {:?} {:?}", c.uid, c.relation, c.nickname);
				}
			}
			EngineAction::Listen { .. } => {}
		}
		Ok(false)
	}

	/// Our echo of a private message proves it was sent.
	fn transfer_done(&mut self, size: u64) -> Result<bool> {
		if let EngineAction::Files {
			command: FilesCommand::Upload { file, channel, path, share: true, .. },
		} = &self.args.action
		{
			let name = file.file_name().context("file name")?.to_string_lossy().into_owned();
			let path = path.clone().unwrap_or_else(|| format!("/{name}"));
			let (dir, name) = match path.rsplit_once('/') {
				Some((dir, name)) => (if dir.is_empty() { "/" } else { dir }, name),
				None => ("/", path.as_str()),
			};
			let link = voelin_model::FileRef {
				server_uid: self.server_uid.clone(),
				channel: *channel,
				path: dir.to_owned(),
				name: name.to_owned(),
				size: Some(size),
				..Default::default()
			};
			let target = ChatTarget::Channel(*channel);
			let text = link.to_bbcode();
			println!("share {text}");
			self.send(Command::SendChat { session: SESSION, target, text });
			self.finish_at = Some(Instant::now() + Duration::from_secs(1));
			return Ok(false);
		}
		Ok(true)
	}
}

async fn contacts(
	engine: &Engine,
	command: &ContactsCommand,
	events: &mut tokio::sync::broadcast::Receiver<Event>,
	deadline: Instant,
) -> Result<()> {
	// The stored contacts are loaded first.
	let _ = timeout_at(deadline, async {
		while let Ok(e) = events.recv().await {
			if matches!(e, Event::ContactsChanged { .. }) {
				break;
			}
		}
	})
	.await;
	match command {
		ContactsCommand::Set { uid, relation, nickname, note } => {
			let relation = match relation.as_str() {
				"friend" => Relation::Friend,
				"blocked" => Relation::Blocked,
				_ => Relation::Neutral,
			};
			let contact = Contact {
				nickname: nickname.clone(),
				relation,
				note: note.clone(),
				..Contact::new(uid.clone())
			};
			engine.send(Command::SetContact { contact: Box::new(contact) });
		}
		ContactsCommand::Rm { uid } => engine.send(Command::RemoveContact { uid: uid.clone() }),
		ContactsCommand::Ls => {}
	}
	let changed = timeout_at(deadline, async {
		while let Ok(e) = events.recv().await {
			if let Event::ContactsChanged { contacts } = e {
				return Some(contacts);
			}
		}
		None
	})
	.await
	.ok()
	.flatten()
	.context("no contacts update")?;
	for c in changed.iter() {
		println!("contact {} {:?} {:?} {:?}", c.uid, c.relation, c.nickname, c.note);
	}
	Ok(())
}

fn md5_of(data: &[u8]) -> md5::Digest {
	md5::compute(data)
}
