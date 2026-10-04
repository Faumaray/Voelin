//! Settings through the engine: commands, change events, attaching stored
//! settings.

use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast::Receiver;
use tokio::time::timeout;
use voelin_core::settings::{STREAM_CODEC, STREAM_FPS, Settings, Source};
use voelin_core::{Command, Engine, Event};

async fn next_setting_event(rx: &mut Receiver<Event>) -> Event {
	timeout(Duration::from_secs(5), async {
		loop {
			let e = rx.recv().await.unwrap();
			if matches!(e, Event::SettingChanged { .. } | Event::SettingRejected { .. }) {
				return e;
			}
		}
	})
	.await
	.expect("no settings event")
}

#[tokio::test]
async fn commands_and_change_events() {
	let engine = Engine::start();
	let mut events = engine.subscribe();

	engine.send(Command::SetSetting { key: "stream.fps".into(), value: json!(144) });
	let Event::SettingChanged { key } = next_setting_event(&mut events).await else {
		panic!("expected a change")
	};
	assert_eq!(key, "stream.fps");
	assert_eq!(engine.settings().get(&STREAM_FPS), 144);

	engine.send(Command::SetSetting { key: "stream.fps".into(), value: json!(0) });
	let Event::SettingRejected { key, message } = next_setting_event(&mut events).await else {
		panic!("expected a rejection")
	};
	assert_eq!(key, "stream.fps");
	assert!(message.contains("at least 1"), "{message}");
	engine.send(Command::SetSetting { key: "no.such.key".into(), value: json!(1) });
	assert!(matches!(next_setting_event(&mut events).await, Event::SettingRejected { .. }));

	// A change made directly on the settings is reported too.
	engine.settings().set_json("stream.codec", json!("av1")).unwrap();
	assert!(matches!(
		next_setting_event(&mut events).await,
		Event::SettingChanged { key } if key == "stream.codec"
	));

	engine.send(Command::ResetSetting { key: "stream.fps".into() });
	assert!(matches!(next_setting_event(&mut events).await, Event::SettingChanged { .. }));
	assert_eq!(engine.settings().get(&STREAM_FPS), 60);
	assert_eq!(engine.settings().source("stream.fps"), Some(Source::Default));

	// Attached settings replace the engine's: their changes are reported,
	// the old ones' are not.
	let old = engine.settings();
	let attached = Settings::in_memory();
	attached.set_override("stream.fps", json!(30)).unwrap();
	engine.send(Command::AttachSettings(attached.clone()));
	engine.send(Command::SetSetting { key: "stream.codec".into(), value: json!("vp9") });
	assert!(matches!(next_setting_event(&mut events).await, Event::SettingChanged { .. }));
	assert_eq!(attached.get(&STREAM_CODEC), voelin_core::settings::CodecChoice::Vp9);
	assert_eq!(engine.settings().get(&STREAM_FPS), 30);
	old.set_json("stream.fps", json!(5)).unwrap();
	attached.set_json("stream.fps", json!(6)).unwrap();
	assert!(matches!(
		next_setting_event(&mut events).await,
		Event::SettingChanged { key } if key == "stream.fps"
	));
	assert_eq!(engine.settings().get(&STREAM_FPS), 6);
}
