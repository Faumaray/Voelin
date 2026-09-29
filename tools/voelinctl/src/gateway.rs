//! `voelinctl gateway`: talk to a tsgw gateway as a user.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use futures_util::{SinkExt, StreamExt};
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use voelin_gateway_proto::{ClientMsg, Envelope, ServerMsg, sign_challenge};
use voelin_model::{ChatTarget, Presence};

#[derive(Args, Debug, Clone)]
pub struct GatewayArgs {
	/// Gateway URL, e.g. ws://127.0.0.1:7788/v1.
	pub url: String,
	/// Identity file (must have connected to the server with voice once).
	#[arg(long, env = "VOELINCTL_IDENTITY")]
	pub identity: PathBuf,
	/// Receive presence and print the tree.
	#[arg(long)]
	pub presence: bool,
	/// Open a chat: `server` or `channel:<cid>`. Repeatable.
	#[arg(long = "open")]
	pub open: Vec<String>,
	/// Send a message: `<target>=<text>`, e.g. `channel:1=hello`.
	#[arg(long = "send")]
	pub send: Vec<String>,
	/// Print stored history of a target.
	#[arg(long)]
	pub history: Option<String>,
	/// Stop after this many seconds.
	#[arg(long, default_value_t = 10)]
	pub seconds: u64,
	/// Exit successfully when a chat message containing this text arrives.
	#[arg(long)]
	pub expect: Option<String>,
	/// Exit successfully when a client with this nickname is present.
	#[arg(long)]
	pub expect_client: Option<String>,

	// Through the engine (voelin-core), as the app uses the gateway.
	/// Run an engine session instead of a raw connection: chat history,
	/// requests, the stream directory. Prints one line per event.
	#[arg(long)]
	pub engine: bool,
	/// Chat history database (engine; default: in memory).
	#[arg(long, requires = "engine")]
	pub db: Option<PathBuf>,
	/// Also connect voice to this server address with the same identity (engine).
	#[arg(long, requires = "engine")]
	pub voice: Option<String>,
	/// Voice nickname (engine).
	#[arg(long, default_value = "voelinctl", requires = "engine")]
	pub nick: String,
	/// Stream media over 127.0.0.1 only (engine, both ends on this machine).
	#[arg(long, requires = "engine")]
	pub loopback: bool,
	/// Open a chat and print its history (engine): `server`, `channel:<cid>`
	/// or `private:<uid>`. Repeatable.
	#[arg(long = "chat", requires = "engine")]
	pub chat: Vec<String>,
	/// Page back this many times in each `--chat` (engine).
	#[arg(long, default_value_t = 0, requires = "engine")]
	pub older: u32,
	/// A gateway request as JSON, sent after login (engine), e.g.
	/// `{"config_get":{"key":"relay.pinned_channels"}}`, `"config_list"`,
	/// `{"perm_set":{"action":"pin","rule":{"everyone":true}}}`. Repeatable.
	#[arg(long = "request", requires = "engine")]
	pub request: Vec<String>,
	/// Post, pin, react, start a topic from the post, post into it and read
	/// it back in this chat, checking each step (engine).
	#[arg(long, requires = "engine")]
	pub roundtrip: Option<String>,
	/// With `--voice` (TeamSpeak 6): stream synthetic frames this long; the
	/// stream registers itself in the gateway's directory (engine).
	#[arg(long, requires = "voice")]
	pub stream_seconds: Option<u64>,
	/// Title of that stream.
	#[arg(long, default_value = "voelinctl", requires = "stream_seconds")]
	pub stream_title: String,
	/// With `--voice`: watch the stream of this client once it is found
	/// (from the server or the gateway's directory) (engine).
	#[arg(long, requires = "voice")]
	pub watch_streamer: Option<String>,
	/// Exit successfully once this many video frames of the watched stream arrived.
	#[arg(long, requires = "watch_streamer")]
	pub expect_frames: Option<u64>,
	/// An engine setting, e.g. `chat.history_page=20`. Repeatable.
	#[arg(long = "set", value_name = "KEY=VALUE", requires = "engine")]
	pub set: Vec<String>,
}

pub fn parse_target(s: &str) -> Result<ChatTarget> {
	match s.split_once(':') {
		None if s == "server" => Ok(ChatTarget::Server),
		Some(("channel", cid)) => Ok(ChatTarget::Channel(cid.parse().context("channel id")?)),
		Some(("private", uid)) => Ok(ChatTarget::Private(uid.to_owned())),
		_ => bail!("target must be `server`, `channel:<cid>` or `private:<uid>`, got {s:?}"),
	}
}

/// [`parse_target`] back.
pub fn target_name(target: &ChatTarget) -> String {
	match target {
		ChatTarget::Server => "server".into(),
		ChatTarget::Channel(cid) => format!("channel:{cid}"),
		ChatTarget::Private(uid) => format!("private:{uid}"),
	}
}

pub async fn run(args: GatewayArgs) -> Result<()> {
	if args.engine {
		return crate::engine::run(args).await;
	}
	let identity = crate::identity::load(&args.identity)?;
	let mut request = args.url.as_str().into_client_request()?;
	request
		.headers_mut()
		.insert("Sec-WebSocket-Protocol", voelin_gateway_proto::SUBPROTOCOL.parse()?);
	let (mut ws, _) = tokio_tungstenite::connect_async(request)
		.await
		.with_context(|| format!("failed to connect to {}", args.url))?;
	let deadline = Instant::now() + Duration::from_secs(args.seconds);

	macro_rules! send {
		($id:expr, $msg:expr) => {
			ws.send(Message::Text(serde_json::to_string(&Envelope::with_id($id, $msg))?.into()))
				.await?
		};
	}

	// Hello -> Auth -> AuthOk
	let (gateway_id, server_uid, nonce) = match next(&mut ws, deadline).await? {
		ServerMsg::Hello { gateway_id, server_uid, nonce, server_name, .. } => {
			eprintln!("gateway for {server_name:?}");
			(gateway_id, server_uid, nonce)
		}
		other => bail!("expected hello, got {other:?}"),
	};
	let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
	let key = identity.key();
	send!(
		1,
		ClientMsg::Auth {
			omega: key.to_pub().to_ts(),
			key_offset: identity.counter(),
			ts,
			signature: sign_challenge(key, &gateway_id, &server_uid, &nonce, ts),
			nickname: String::new(),
		}
	);
	match next(&mut ws, deadline).await? {
		ServerMsg::AuthOk { uid, .. } => eprintln!("logged in as {uid}"),
		ServerMsg::Error { code, message } => bail!("login refused: {code:?}: {message}"),
		other => bail!("unexpected {other:?}"),
	}

	let mut id = 1;
	if args.presence {
		id += 1;
		send!(id, ClientMsg::SubscribePresence);
	}
	for target in &args.open {
		id += 1;
		send!(id, ClientMsg::OpenChat { target: parse_target(target)? });
	}
	for spec in &args.send {
		let (target, text) =
			spec.split_once('=').ok_or_else(|| anyhow!("--send needs <target>=<text>"))?;
		id += 1;
		send!(id, ClientMsg::SendChat { target: parse_target(target)?, text: text.to_string() });
	}
	if let Some(target) = &args.history {
		id += 1;
		send!(id, ClientMsg::History { target: parse_target(target)?, before: None, limit: 50 });
	}

	let mut presence = Presence::default();
	loop {
		let msg = match next(&mut ws, deadline).await {
			Ok(m) => m,
			Err(e) if e.to_string() == "timeout" => break,
			Err(e) => return Err(e),
		};
		match msg {
			ServerMsg::PresenceSnapshot { snapshot, .. } => {
				presence = Presence::from_snapshot(snapshot);
				println!(
					"presence: {} channels, {} clients",
					presence.channels.len(),
					presence.clients.len()
				);
				for c in presence.clients.values() {
					let channel =
						presence.channels.get(&c.channel).map(|c| c.name.as_str()).unwrap_or("?");
					println!("  {} in #{channel}", c.nickname);
				}
			}
			ServerMsg::PresenceDelta { seq, delta } => {
				presence.apply(&delta);
				println!("delta {seq}: {}", serde_json::to_string(&delta)?);
			}
			ServerMsg::ChatEvent { id, message } => {
				println!(
					"chat #{id} {:?} {}: {}",
					message.target, message.author_name, message.text
				);
				if args.expect.as_ref().is_some_and(|e| message.text.contains(e.as_str())) {
					return Ok(());
				}
			}
			ServerMsg::History { messages } => {
				println!("history: {} messages", messages.len());
				for m in messages {
					println!("  #{} {}: {}", m.id, m.message.author_name, m.message.text);
				}
			}
			ServerMsg::Error { code, message } => println!("error: {code:?}: {message}"),
			ServerMsg::Ok | ServerMsg::Pong => {}
			other => println!("{other:?}"),
		}
		if let Some(nick) = &args.expect_client
			&& presence.clients.values().any(|c| &c.nickname == nick)
		{
			return Ok(());
		}
	}
	if args.expect.is_some() || args.expect_client.is_some() {
		bail!("expected event did not arrive");
	}
	Ok(())
}

async fn next<S>(ws: &mut S, deadline: Instant) -> Result<ServerMsg>
where
	S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
	loop {
		let msg = timeout_at(deadline, ws.next()).await.map_err(|_| anyhow!("timeout"))?;
		match msg {
			Some(Ok(Message::Text(text))) => {
				let env: Envelope<ServerMsg> = serde_json::from_str(text.as_str())?;
				return Ok(env.msg);
			}
			Some(Ok(Message::Close(_))) | None => bail!("gateway closed the connection"),
			Some(Ok(_)) => continue,
			Some(Err(e)) => return Err(e.into()),
		}
	}
}
