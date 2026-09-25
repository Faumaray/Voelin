//! The gateway source: presence and relayed chat through `tsgw`.

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::info;
use tsc_gateway_proto::{ClientMsg, Envelope, ServerMsg, sign_challenge};
use tsc_model::{ChatMessage, ChatTarget, Presence};

pub(crate) enum GatewayCmd {
	OpenChat(ChatTarget),
	CloseChat(ChatTarget),
	SendChat(ChatTarget, String),
	Stop,
}

pub(crate) enum GatewayEvent {
	Connected,
	Presence(Presence),
	Chat(ChatMessage),
	Error(String),
	Disconnected(Option<String>),
}

pub(crate) async fn run(
	url: String,
	identity: tsclientlib::Identity,
	mut commands: mpsc::UnboundedReceiver<GatewayCmd>,
	events: mpsc::UnboundedSender<GatewayEvent>,
) {
	let reason =
		run_inner(&url, &identity, &mut commands, &events).await.err().map(|e| e.to_string());
	let _ = events.send(GatewayEvent::Disconnected(reason));
}

async fn run_inner(
	url: &str,
	identity: &tsclientlib::Identity,
	commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	events: &mpsc::UnboundedSender<GatewayEvent>,
) -> anyhow::Result<()> {
	let mut request = url.into_client_request()?;
	request.headers_mut().insert("Sec-WebSocket-Protocol", tsc_gateway_proto::SUBPROTOCOL.parse()?);
	let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
	let mut next_id = 1u64;
	let mut presence = Presence::default();

	macro_rules! send {
		($msg:expr) => {{
			next_id += 1;
			let env = Envelope::with_id(next_id, $msg);
			ws.send(Message::Text(serde_json::to_string(&env)?.into())).await?;
		}};
	}

	loop {
		tokio::select! {
			msg = ws.next() => {
				let text = match msg {
					Some(Ok(Message::Text(t))) => t,
					Some(Ok(Message::Close(_))) | None => return Ok(()),
					Some(Ok(_)) => continue,
					Some(Err(e)) => return Err(e.into()),
				};
				let env: Envelope<ServerMsg> = serde_json::from_str(text.as_str())?;
				match env.msg {
					ServerMsg::Hello { gateway_id, server_uid, nonce, server_name, .. } => {
						let ts = std::time::SystemTime::now()
							.duration_since(std::time::UNIX_EPOCH)?
							.as_secs() as i64;
						let key = identity.key();
						send!(ClientMsg::Auth {
							omega: key.to_pub().to_ts(),
							key_offset: identity.counter(),
							ts,
							signature: sign_challenge(key, &gateway_id, &server_uid, &nonce, ts),
							nickname: String::new(),
						});
						info!(%server_name, "gateway hello");
					}
					ServerMsg::AuthOk { .. } => {
						let _ = events.send(GatewayEvent::Connected);
						send!(ClientMsg::SubscribePresence);
					}
					ServerMsg::PresenceSnapshot { snapshot, .. } => {
						presence = Presence::from_snapshot(snapshot);
						let _ = events.send(GatewayEvent::Presence(presence.clone()));
					}
					ServerMsg::PresenceDelta { delta, .. } => {
						presence.apply(&delta);
						let _ = events.send(GatewayEvent::Presence(presence.clone()));
					}
					ServerMsg::ChatEvent { message, .. } => {
						let _ = events.send(GatewayEvent::Chat(message));
					}
					ServerMsg::Error { code, message } => {
						let _ = events.send(GatewayEvent::Error(format!("{code:?}: {message}")));
						if matches!(
							code,
							tsc_gateway_proto::ErrorCode::AuthFailed
								| tsc_gateway_proto::ErrorCode::UnknownIdentity
								| tsc_gateway_proto::ErrorCode::LevelTooLow
								| tsc_gateway_proto::ErrorCode::Banned
						) {
							anyhow::bail!("gateway refused login: {message}");
						}
					}
					_ => {}
				}
			}
			cmd = commands.recv() => match cmd {
				None | Some(GatewayCmd::Stop) => return Ok(()),
				Some(GatewayCmd::OpenChat(target)) => send!(ClientMsg::OpenChat { target }),
				Some(GatewayCmd::CloseChat(target)) => send!(ClientMsg::CloseChat { target }),
				Some(GatewayCmd::SendChat(target, text)) => send!(ClientMsg::SendChat { target, text }),
			},
		}
	}
}
