//! The gateway source: presence and relayed chat through `tsgw`, and the
//! typed client for everything else a gateway offers (pins, reactions,
//! topics, events, the stream directory, the activity feed,
//! administration).
//!
//! [`GatewayClient`] (from `voelin-gateway-proto`, feature `client`) has one
//! async method per request and delivers pushes as [`Push`];
//! [`GatewayClient::capabilities`] says which features the gateway and user
//! have, so the UI can hide the rest. The data types (`HistoryQuery`,
//! `PinInfo`, `EventSpec`, `StreamEntry`, …) are in `voelin_gateway_proto`.
//! [`connect`] logs in with the user's TeamSpeak identity. The engine uses
//! [`run`] for presence and chat.

use tokio::sync::mpsc;
use tracing::info;
use voelin_gateway_proto::client::Login;
pub use voelin_gateway_proto::client::{ClientError, GatewayClient, Push};
use voelin_gateway_proto::{ClientMsg, ErrorCode};
use voelin_model::{ChatMessage, ChatTarget, Presence};

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

/// Connect to a gateway (`ws://…/v1` or `wss://…/v1`) and log in with a
/// TeamSpeak identity.
pub async fn connect(
	url: &str,
	identity: &tsclientlib::Identity,
) -> Result<(GatewayClient, mpsc::UnboundedReceiver<Push>), ClientError> {
	let login = Login::Identity { key: identity.key().clone(), key_offset: identity.counter() };
	voelin_gateway_proto::client::connect(url, login).await
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
	let (client, mut pushes) = match connect(url, identity).await {
		Ok(connected) => connected,
		Err(ClientError::Gateway { code, message }) => {
			let _ = events.send(GatewayEvent::Error(format!("{code:?}: {message}")));
			anyhow::bail!("gateway refused login: {message}");
		}
		Err(e) => return Err(e.into()),
	};
	info!(server_name = %client.info().server_name, capabilities = ?client.capabilities(), "gateway connected");
	let _ = events.send(GatewayEvent::Connected);
	client.subscribe_presence()?;
	let mut presence = Presence::default();

	loop {
		tokio::select! {
			push = pushes.recv() => match push {
				None | Some(Push::Disconnected(None)) => return Ok(()),
				Some(Push::Disconnected(Some(reason))) => anyhow::bail!(reason),
				Some(Push::PresenceSnapshot(snapshot)) => {
					presence = Presence::from_snapshot(snapshot);
					let _ = events.send(GatewayEvent::Presence(presence.clone()));
				}
				Some(Push::PresenceDelta(delta)) => {
					presence.apply(&delta);
					let _ = events.send(GatewayEvent::Presence(presence.clone()));
				}
				Some(Push::Chat { message, .. }) => {
					let _ = events.send(GatewayEvent::Chat(message));
				}
				Some(Push::Message(entry)) => {
					let _ = events.send(GatewayEvent::Chat(entry.message));
				}
				Some(Push::Error { code, message }) => {
					let _ = events.send(GatewayEvent::Error(format!("{code:?}: {message}")));
					if code == ErrorCode::NotAuthenticated {
						anyhow::bail!("gateway session lost: {message}");
					}
				}
				Some(_) => {}
			},
			cmd = commands.recv() => match cmd {
				None | Some(GatewayCmd::Stop) => return Ok(()),
				Some(GatewayCmd::OpenChat(target)) => client.send(ClientMsg::OpenChat { target })?,
				Some(GatewayCmd::CloseChat(target)) => client.send(ClientMsg::CloseChat { target })?,
				Some(GatewayCmd::SendChat(target, text)) => {
					client.send(ClientMsg::SendChat { target, text })?;
				}
			},
		}
	}
}
