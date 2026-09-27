//! Against a real TeamSpeak server and a running tsgw (ignored by default):
//!
//! ```sh
//! cargo run -p voelin-gateway -- --config dev/tsgw-ts3.toml &
//! TSGW_LIVE_URL=ws://127.0.0.1:7787/v1 \
//! TSGW_LIVE_IDENTITY=dev/.state/ts3-gateway-user.json \
//! TSGW_LIVE_ADMIN=dev/.state/ts3-admin.json \
//! cargo test -p voelin-gateway live -- --ignored --nocapture
//! ```
//!
//! The identities must have connected to the server with voice once
//! (`scripts/it-smoke.sh` leaves such files in `dev/.state`); the admin one
//! must be in the Server Admin group.

use serde_json::json;
use tsproto_types::crypto::EccKeyPrivP256;
use voelin_gateway_proto::client::{GatewayClient, Login, Push, connect};
use voelin_gateway_proto::{
	Action, EventKind, EventSpec, HistoryQuery, PermRule, RsvpStatus, StreamSpec, feature,
};
use voelin_model::ChatTarget;

use super::expect;
use crate::hub::now_ms;

async fn login(var: &str) -> (GatewayClient, tokio::sync::mpsc::UnboundedReceiver<Push>) {
	let url = std::env::var("TSGW_LIVE_URL").expect("TSGW_LIVE_URL");
	let path = std::env::var(var).unwrap_or_else(|_| panic!("{var}"));
	let file: serde_json::Value =
		serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
	let key = EccKeyPrivP256::import_str(file["key"].as_str().unwrap()).unwrap();
	let key_offset = file["counter"].as_u64().unwrap();
	connect(&url, Login::Identity { key, key_offset }).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a running tsgw and TeamSpeak server"]
async fn live_gateway() {
	let (user, mut user_rx) = login("TSGW_LIVE_IDENTITY").await;
	let (admin, _admin_rx) = login("TSGW_LIVE_ADMIN").await;
	println!("capabilities: user {:?}, admin {:?}", user.capabilities(), admin.capabilities());
	assert!(admin.has(feature::ADMIN), "the admin identity is not a Server Admin");
	let lobby = ChatTarget::Channel(1);
	user.enable(Vec::new()).await.unwrap();
	user.open_chat(lobby.clone()).await.unwrap();
	admin.open_chat(lobby.clone()).await.unwrap();

	let token = format!("live-{}", now_ms());
	let posted = user.post(lobby.clone(), format!("hello {token}"), None).await.unwrap();
	println!("posted #{} rev {}", posted.id, posted.rev);
	admin.pin(posted.id).await.unwrap();
	expect(&mut user_rx, "pinned", |p| matches!(p, Push::Pinned { .. })).await;
	user.react(posted.id, "🎉".into()).await.unwrap();
	let topic =
		user.create_topic(lobby.clone(), format!("topic {token}"), Some(posted.id)).await.unwrap();
	let in_topic = user.post(lobby.clone(), "in the topic".into(), Some(topic.id)).await.unwrap();
	let q = HistoryQuery { topic: Some(topic.id), ..HistoryQuery::latest(lobby.clone(), None) };
	assert_eq!(user.history(q).await.unwrap().messages[0].id, in_topic.id);
	let page = user.history(HistoryQuery::latest(lobby.clone(), Some(5))).await.unwrap();
	let entry = page.messages.iter().find(|e| e.id == posted.id).unwrap();
	println!("history entry: pinned {} reactions {:?}", entry.pinned, entry.reactions);
	assert!(entry.pinned && entry.reactions[0].me);

	user.subscribe_events().await.unwrap();
	let event = admin
		.create_event(EventSpec {
			title: format!("event {token}"),
			start_ms: now_ms() + 3_600_000,
			kind: EventKind::Stream,
			channel: Some(1),
			..Default::default()
		})
		.await
		.unwrap();
	let answered = user.rsvp(event.id, Some(RsvpStatus::Maybe)).await.unwrap();
	assert_eq!(answered.maybe, 1);
	admin.delete_event(event.id).await.unwrap();

	user.subscribe_streams().await.unwrap();
	let stream = user
		.register_stream(StreamSpec {
			stream_id: token.clone(),
			title: "live test".into(),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(user.streams().await.unwrap().iter().filter(|s| s.id == stream.id).count(), 1);
	user.unregister_stream(stream.id).await.unwrap();

	let before = admin.config_get("quota.history_page".into()).await.unwrap();
	admin.config_set("quota.history_page".into(), json!(1)).await.unwrap();
	assert_eq!(
		user.history(HistoryQuery::latest(lobby.clone(), None)).await.unwrap().messages.len(),
		1
	);
	admin.config_reset("quota.history_page".into()).await.unwrap();
	println!("quota.history_page back to {} ({:?})", before.value, before.source);
	admin.perm_set(Action::React, PermRule::default()).await.unwrap();
	assert!(user.react(posted.id, "👍".into()).await.is_err());
	admin.perm_reset(Action::React).await.unwrap();
	admin.unpin(posted.id).await.unwrap();
	let (feed, _) = user.activity(None, Some(10)).await.unwrap();
	println!("activity: {:?}", feed.iter().map(|e| &e.text).collect::<Vec<_>>());
}
