//! One WebSocket client.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use base64::prelude::*;
use rand::RngExt;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info};
use voelin_gateway_proto::{ClientMsg, Envelope, ErrorCode, HistoryEntry, ServerMsg};
use voelin_model::{ChatTarget, Presence};
use voelin_observer::ObserverEvent;

use crate::hub::{Denied, Hub, User};

/// At most this many posts per window per session.
const POST_LIMIT: (usize, Duration) = (5, Duration::from_secs(5));

struct Session {
	hub: Arc<Hub>,
	nonce: String,
	user: Option<User>,
	presence: Option<(Presence, u64)>,
	open_chats: HashSet<ChatTarget>,
	posts: Vec<Instant>,
}

pub async fn run(hub: Arc<Hub>, mut socket: WebSocket) {
	let nonce_bytes: [u8; 24] = rand::rng().random();
	let mut session = Session {
		nonce: BASE64_URL_SAFE_NO_PAD.encode(nonce_bytes),
		hub: hub.clone(),
		user: None,
		presence: None,
		open_chats: HashSet::new(),
		posts: Vec::new(),
	};
	let hello = ServerMsg::Hello {
		gateway_id: hub.gateway_id.clone(),
		server_uid: hub.server_uid.clone(),
		server_name: hub.server_name.clone(),
		nonce: session.nonce.clone(),
		capabilities: vec!["presence".into(), "relay".into(), "history".into()],
	};
	if send(&mut socket, None, hello).await.is_err() {
		return;
	}
	let mut observer = hub.observer.subscribe();
	let mut chat = hub.subscribe_chat();

	loop {
		tokio::select! {
			msg = socket.recv() => {
				let Some(Ok(msg)) = msg else { break };
				let text = match msg {
					Message::Text(t) => t,
					Message::Close(_) => break,
					_ => continue,
				};
				let replies = match serde_json::from_str::<Envelope<ClientMsg>>(text.as_str()) {
					Ok(env) => session.handle(env.msg).await.into_iter().map(|m| (env.id, m)).collect(),
					Err(e) => vec![(None, error(ErrorCode::BadRequest, &e.to_string()))],
				};
				for (id, reply) in replies {
					if send(&mut socket, id, reply).await.is_err() {
						break;
					}
				}
			}
			event = observer.recv(), if session.presence.is_some() => {
				let resync = matches!(event, Err(RecvError::Lagged(_)) | Ok(ObserverEvent::Snapshot(_)));
				if let Err(RecvError::Closed) = event {
					break;
				}
				for msg in session.presence_update(resync) {
					if send(&mut socket, None, msg).await.is_err() {
						break;
					}
				}
			}
			event = chat.recv() => {
				match event {
					Ok((id, message)) if session.open_chats.contains(&message.target) => {
						if send(&mut socket, None, ServerMsg::ChatEvent { id, message }).await.is_err() {
							break;
						}
					}
					Ok(_) | Err(RecvError::Lagged(_)) => {}
					Err(RecvError::Closed) => break,
				}
			}
		}
	}
	session.close();
}

async fn send(socket: &mut WebSocket, id: Option<u64>, msg: ServerMsg) -> Result<(), axum::Error> {
	let env = Envelope { v: voelin_gateway_proto::VERSION, id, msg };
	let text = serde_json::to_string(&env).expect("messages serialize");
	socket.send(Message::Text(text.into())).await
}

fn error(code: ErrorCode, message: &str) -> ServerMsg {
	ServerMsg::Error { code, message: message.to_string() }
}

impl Session {
	async fn handle(&mut self, msg: ClientMsg) -> Vec<ServerMsg> {
		match self.handle_inner(msg).await {
			Ok(msgs) => msgs,
			Err(Denied(code, message)) => vec![error(code, &message)],
		}
	}

	fn user(&self) -> Result<&User, Denied> {
		self.user.as_ref().ok_or(Denied(ErrorCode::NotAuthenticated, "log in first".into()))
	}

	async fn handle_inner(&mut self, msg: ClientMsg) -> Result<Vec<ServerMsg>, Denied> {
		match msg {
			ClientMsg::Ping => Ok(vec![ServerMsg::Pong]),
			ClientMsg::Auth { omega, key_offset, ts, signature, .. } => {
				let (user, token, token_expires) =
					self.hub.authenticate(&omega, key_offset, ts, &signature, &self.nonce).await?;
				info!(uid = %user.uid, nickname = %user.nickname, "user logged in");
				let uid = user.uid.clone();
				self.user = Some(user);
				Ok(vec![ServerMsg::AuthOk { uid, token, token_expires, capabilities: Vec::new() }])
			}
			ClientMsg::Resume { token, .. } => {
				let (user, token, token_expires) = self.hub.resume(&token).await?;
				let uid = user.uid.clone();
				self.user = Some(user);
				Ok(vec![ServerMsg::AuthOk { uid, token, token_expires, capabilities: Vec::new() }])
			}
			ClientMsg::SubscribePresence => {
				self.user()?;
				self.presence = None;
				self.presence = Some((Presence::default(), 0));
				Ok(self.presence_update(true))
			}
			ClientMsg::UnsubscribePresence => {
				self.presence = None;
				Ok(vec![ServerMsg::Ok])
			}
			ClientMsg::OpenChat { target } => {
				let user = self.user()?.clone();
				if self.open_chats.contains(&target) {
					return Ok(vec![ServerMsg::Ok]);
				}
				match &target {
					ChatTarget::Server => {}
					ChatTarget::Channel(cid) => {
						if !self.hub.channel_access(&user, *cid).await?.read {
							return Err(Denied(
								ErrorCode::Forbidden,
								"no access to this channel".into(),
							));
						}
						self.hub.add_reader(*cid).await?;
					}
					ChatTarget::Private(_) => {
						return Err(Denied(
							ErrorCode::BadRequest,
							"private chat is not relayed".into(),
						));
					}
				}
				debug!(uid = %user.uid, ?target, "chat opened");
				self.open_chats.insert(target);
				Ok(vec![ServerMsg::Ok])
			}
			ClientMsg::CloseChat { target } => {
				if self.open_chats.remove(&target)
					&& let ChatTarget::Channel(cid) = target
				{
					self.hub.remove_reader(cid);
				}
				Ok(vec![ServerMsg::Ok])
			}
			ClientMsg::SendChat { target, text } => {
				let user = self.user()?.clone();
				self.posts.retain(|t| t.elapsed() < POST_LIMIT.1);
				if self.posts.len() >= POST_LIMIT.0 {
					return Err(Denied(ErrorCode::Unavailable, "slow down".into()));
				}
				self.posts.push(Instant::now());
				self.hub.post(&user, &target, &text).await?;
				Ok(vec![ServerMsg::Ok])
			}
			ClientMsg::History { target, before, limit } => {
				let user = self.user()?.clone();
				if let ChatTarget::Channel(cid) = target
					&& !self.hub.channel_access(&user, cid).await?.read
				{
					return Err(Denied(ErrorCode::Forbidden, "no access to this channel".into()));
				}
				let Some(db) = &self.hub.db else {
					return Ok(vec![ServerMsg::History { messages: Vec::new() }]);
				};
				let messages = db
					.history(&target, before, limit.min(200))
					.map_err(|e| Denied(ErrorCode::Internal, e.to_string()))?
					.into_iter()
					.map(|(id, message)| HistoryEntry::new(id, message))
					.collect();
				Ok(vec![ServerMsg::History { messages }])
			}
		}
	}

	/// Send what changed in this user's view of the server (or a snapshot).
	fn presence_update(&mut self, snapshot: bool) -> Vec<ServerMsg> {
		let Some(user) = &self.user else { return Vec::new() };
		let Some((last, seq)) = &mut self.presence else { return Vec::new() };
		let current = self.hub.filtered_presence(user);
		if snapshot {
			*seq = 0;
			*last = current.clone();
			return vec![ServerMsg::PresenceSnapshot { seq: 0, snapshot: current.snapshot() }];
		}
		let deltas = last.diff(&current);
		*last = current;
		deltas
			.into_iter()
			.map(|delta| {
				*seq += 1;
				ServerMsg::PresenceDelta { seq: *seq, delta }
			})
			.collect()
	}

	fn close(&mut self) {
		for target in self.open_chats.drain() {
			if let ChatTarget::Channel(cid) = target {
				self.hub.remove_reader(cid);
			}
		}
	}
}
