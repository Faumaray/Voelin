//! The query source: own ServerQuery credentials, in-process observer and relays.

use tokio::sync::{broadcast, mpsc};
use voelin_model::{ChatMessage, ChatTarget, Presence};
use voelin_observer::{
	Observer, ObserverConfig, ObserverEvent, RelayConfig, RelayEvent, RelayPool,
};

pub(crate) enum QueryCmd {
	OpenChat(ChatTarget),
	CloseChat(ChatTarget),
	SendChat { target: ChatTarget, nick: String, text: String },
	Stop,
}

pub(crate) enum QueryEvent {
	Presence(Presence),
	Chat(ChatMessage),
	Error(String),
	Disconnected,
}

pub(crate) async fn run(
	connect: voelin_query::Connect,
	mut commands: mpsc::UnboundedReceiver<QueryCmd>,
	events: mpsc::UnboundedSender<QueryEvent>,
) {
	let observer = Observer::spawn(ObserverConfig::new(connect.clone()));
	let relays = RelayPool::new(RelayConfig::new(connect));
	let mut observed = observer.subscribe();
	let mut relayed = relays.subscribe();
	let mut open = std::collections::HashSet::new();
	loop {
		tokio::select! {
			e = observed.recv() => match e {
				Ok(ObserverEvent::Snapshot(_)) | Ok(ObserverEvent::Delta(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {
					let presence = observer.presence().read().unwrap().clone();
					let _ = events.send(QueryEvent::Presence(presence));
				}
				Ok(ObserverEvent::Chat(msg)) => {
					if open.contains(&msg.target) {
						let _ = events.send(QueryEvent::Chat(msg));
					}
				}
				Ok(ObserverEvent::Disconnected(reason)) => {
					let _ = events.send(QueryEvent::Error(format!("query connection lost: {reason}")));
				}
				Err(broadcast::error::RecvError::Closed) => break,
			},
			e = relayed.recv() => match e {
				Ok(RelayEvent::Message(msg)) => {
					let _ = events.send(QueryEvent::Chat(msg));
				}
				Ok(RelayEvent::Closed { channel, reason }) => {
					let _ = events.send(QueryEvent::Error(format!("relay for channel {channel} closed: {reason}")));
				}
				Err(_) => {}
			},
			cmd = commands.recv() => match cmd {
				None | Some(QueryCmd::Stop) => break,
				Some(QueryCmd::OpenChat(target)) => {
					if let ChatTarget::Channel(cid) = target
						&& let Err(e) = relays.open(cid).await
					{
						let _ = events.send(QueryEvent::Error(e.to_string()));
						continue;
					}
					open.insert(target);
				}
				Some(QueryCmd::CloseChat(target)) => {
					if let ChatTarget::Channel(cid) = target {
						relays.close(cid).await;
					}
					open.remove(&target);
				}
				Some(QueryCmd::SendChat { target, nick, text }) => {
					let result = match &target {
						ChatTarget::Channel(cid) => relays.send(*cid, &nick, &text).await,
						_ => Err(voelin_query::Error::Unsupported("only channel chat is relayed with own credentials")),
					};
					if let Err(e) = result {
						let _ = events.send(QueryEvent::Error(e.to_string()));
					}
				}
			},
		}
	}
	relays.close_all().await;
	let _ = events.send(QueryEvent::Disconnected);
}
