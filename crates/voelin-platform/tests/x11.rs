//! Global hotkeys against a real X server, typing through XTEST.
//!
//! Needs `VOELIN_X11_TEST_DISPLAY` naming a display to use (e.g. an Xvfb
//! started for the test, as CI does); skipped otherwise.

#![cfg(all(unix, not(target_os = "macos"), feature = "x11"))]

use std::time::Duration;

use tokio::time::timeout;
use voelin_platform::{HotkeyEvent, HotkeyEvents, HotkeyManager};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
	BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
	KEY_RELEASE_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

struct Typist {
	conn: RustConnection,
	root: u32,
}

impl Typist {
	fn new(display: &str) -> Self {
		let (conn, screen) = x11rb::connect(Some(display)).unwrap();
		let root = conn.setup().roots[screen].root;
		Self { conn, root }
	}

	fn keycode(&self, keysym: u32) -> u8 {
		let setup = self.conn.setup();
		let (min, max) = (setup.min_keycode, setup.max_keycode);
		let map = self.conn.get_keyboard_mapping(min, max - min + 1).unwrap().reply().unwrap();
		let per = map.keysyms_per_keycode as usize;
		let index = map.keysyms.iter().position(|&s| s == keysym).expect("keysym in keymap");
		min + (index / per) as u8
	}

	fn fake(&self, kind: u8, detail: u8) {
		self.conn.xtest_fake_input(kind, detail, 0, self.root, 0, 0, 0).unwrap();
		self.conn.sync().unwrap();
	}

	fn key(&self, keysym: u32, down: bool) {
		let kind = if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
		self.fake(kind, self.keycode(keysym));
	}

	fn button(&self, button: u8, down: bool) {
		self.fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, button);
	}
}

async fn next(events: &mut HotkeyEvents) -> Option<HotkeyEvent> {
	timeout(Duration::from_secs(2), events.recv()).await.ok().flatten()
}

async fn nothing(events: &mut HotkeyEvents) -> bool {
	timeout(Duration::from_millis(300), events.recv()).await.is_err()
}

const XK_CONTROL_L: u32 = 0xffe3;
const XK_F9: u32 = 0xffc6;
const XK_A: u32 = 0x61;

#[tokio::test]
async fn xinput2_press_and_release() {
	let Ok(display) = std::env::var("VOELIN_X11_TEST_DISPLAY") else {
		eprintln!("VOELIN_X11_TEST_DISPLAY not set, skipping");
		return;
	};
	let mut manager = HotkeyManager::x11(&display).unwrap();
	let mut ptt =
		manager.register("ptt", "Push to talk", "Ctrl+F9".parse().unwrap()).await.unwrap();
	let mut plain = manager.register("a", "Plain key", "A".parse().unwrap()).await.unwrap();
	let mut back = manager.register("back", "Mouse", "MouseBack".parse().unwrap()).await.unwrap();
	let typist = Typist::new(&display);

	// F9 alone does not trigger Ctrl+F9.
	typist.key(XK_F9, true);
	typist.key(XK_F9, false);
	assert!(nothing(&mut ptt).await);

	typist.key(XK_CONTROL_L, true);
	typist.key(XK_F9, true);
	assert_eq!(next(&mut ptt).await, Some(HotkeyEvent::Pressed));
	// Still held while Ctrl goes up first.
	typist.key(XK_CONTROL_L, false);
	assert!(nothing(&mut ptt).await);
	typist.key(XK_F9, false);
	assert_eq!(next(&mut ptt).await, Some(HotkeyEvent::Released));

	typist.key(XK_A, true);
	typist.key(XK_A, false);
	assert_eq!(next(&mut plain).await, Some(HotkeyEvent::Pressed));
	assert_eq!(next(&mut plain).await, Some(HotkeyEvent::Released));

	typist.button(8, true);
	typist.button(8, false);
	assert_eq!(next(&mut back).await, Some(HotkeyEvent::Pressed));
	assert_eq!(next(&mut back).await, Some(HotkeyEvent::Released));

	manager.unregister("a").await.unwrap();
	typist.key(XK_A, true);
	typist.key(XK_A, false);
	// The sender is gone: the channel ends.
	assert_eq!(next(&mut plain).await, None);

	// Dropping the manager stops its thread and closes the channels.
	drop(manager);
	assert_eq!(next(&mut ptt).await, None);
}
