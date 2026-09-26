//! X11 backend: XInput2 raw key and button events on the root window.
//!
//! Raw events reach us whichever window has focus, and nothing is grabbed,
//! so the key also reaches the focused application. A thread blocks on the
//! connection; dropping the backend wakes it with a client message to a
//! private input-only window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _, XIEventMask};
use x11rb::protocol::xproto::{
	AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, Window,
	WindowClass,
};
use x11rb::rust_connection::RustConnection;

use super::tracker::Tracker;
use super::{Hotkey, HotkeyEvents, Key, NamedKey};
use crate::{Error, Result};

fn x11_error(e: impl std::fmt::Display) -> Error {
	Error::Backend(format!("X11: {e}"))
}

/// Keycode to keysyms, from the server's keyboard mapping.
struct Keymap {
	min_keycode: u8,
	per_keycode: usize,
	keysyms: Vec<u32>,
}

impl Keymap {
	fn load(conn: &RustConnection) -> Result<Self> {
		let setup = conn.setup();
		let (min, max) = (setup.min_keycode, setup.max_keycode);
		let reply = conn
			.get_keyboard_mapping(min, max - min + 1)
			.map_err(x11_error)?
			.reply()
			.map_err(x11_error)?;
		Ok(Self {
			min_keycode: min,
			per_keycode: reply.keysyms_per_keycode as usize,
			keysyms: reply.keysyms,
		})
	}

	/// The key on this keycode, by the first of its keysyms we know.
	fn key(&self, keycode: u32) -> Option<Key> {
		let index = (keycode.checked_sub(self.min_keycode as u32)? as usize) * self.per_keycode;
		let syms = self.keysyms.get(index..index + self.per_keycode)?;
		syms.iter().find_map(|&s| Key::from_x11_keysym(s))
	}
}

/// Raw (physical) button numbers: 2 middle, 8 back, 9 forward.
fn button_key(button: u32) -> Option<Key> {
	match button {
		2 => Some(Key::Named(NamedKey::MouseMiddle)),
		8 => Some(Key::Named(NamedKey::MouseBack)),
		9 => Some(Key::Named(NamedKey::MouseForward)),
		_ => None,
	}
}

pub(crate) struct X11Backend {
	conn: Arc<RustConnection>,
	wake_window: Window,
	tracker: Arc<Mutex<Tracker>>,
	stop: Arc<AtomicBool>,
}

impl X11Backend {
	/// Connect to `display`, or `$DISPLAY` for `None`.
	pub fn connect(display: Option<&str>) -> Result<Self> {
		let (conn, screen) = x11rb::connect(display).map_err(x11_error)?;
		let root = conn.setup().roots[screen].root;

		let version =
			conn.xinput_xi_query_version(2, 0).map_err(x11_error)?.reply().map_err(x11_error)?;
		if version.major_version < 2 {
			return Err(Error::Backend("X11: XInput 2 is not available".into()));
		}
		let mask = XIEventMask::RAW_KEY_PRESS
			| XIEventMask::RAW_KEY_RELEASE
			| XIEventMask::RAW_BUTTON_PRESS
			| XIEventMask::RAW_BUTTON_RELEASE;
		let masks =
			[xinput::EventMask { deviceid: xinput::Device::ALL_MASTER.into(), mask: vec![mask] }];
		conn.xinput_xi_select_events(root, &masks)
			.map_err(x11_error)?
			.check()
			.map_err(x11_error)?;

		let wake_window = conn.generate_id().map_err(x11_error)?;
		conn.create_window(
			0,
			wake_window,
			root,
			0,
			0,
			1,
			1,
			0,
			WindowClass::INPUT_ONLY,
			0,
			&CreateWindowAux::new(),
		)
		.map_err(x11_error)?
		.check()
		.map_err(x11_error)?;

		let keymap = Keymap::load(&conn)?;
		let conn = Arc::new(conn);
		let tracker = Arc::new(Mutex::new(Tracker::default()));
		let stop = Arc::new(AtomicBool::new(false));
		{
			let (conn, tracker, stop) = (conn.clone(), tracker.clone(), stop.clone());
			thread::Builder::new()
				.name("voelin-hotkeys-x11".into())
				.spawn(move || event_loop(&conn, keymap, &tracker, &stop))
				.map_err(|e| Error::Backend(format!("X11: {e}")))?;
		}
		Ok(Self { conn, wake_window, tracker, stop })
	}

	fn tracker(&self) -> std::sync::MutexGuard<'_, Tracker> {
		self.tracker.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub fn register(&mut self, id: &str, hotkey: Hotkey) -> HotkeyEvents {
		self.tracker().add(id, hotkey)
	}

	pub fn unregister(&mut self, id: &str) {
		self.tracker().remove(id);
	}
}

impl Drop for X11Backend {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		// With an empty event mask the event goes to the window's creator: us.
		let wake = ClientMessageEvent::new(32, self.wake_window, AtomEnum::NONE, [0u32; 5]);
		let _ = self.conn.send_event(false, self.wake_window, EventMask::NO_EVENT, wake);
		let _ = self.conn.flush();
	}
}

fn event_loop(
	conn: &RustConnection,
	mut keymap: Keymap,
	tracker: &Mutex<Tracker>,
	stop: &AtomicBool,
) {
	let lock = || tracker.lock().unwrap_or_else(PoisonError::into_inner);
	loop {
		let event = match conn.wait_for_event() {
			Ok(event) => event,
			Err(error) => {
				warn!(%error, "X11 hotkey connection lost");
				lock().release_all();
				return;
			}
		};
		if stop.load(Ordering::Relaxed) {
			lock().release_all();
			return;
		}
		let (key, pressed) = match event {
			Event::XinputRawKeyPress(e) => (keymap.key(e.detail), true),
			Event::XinputRawKeyRelease(e) => (keymap.key(e.detail), false),
			Event::XinputRawButtonPress(e) => (button_key(e.detail), true),
			Event::XinputRawButtonRelease(e) => (button_key(e.detail), false),
			Event::MappingNotify(_) => {
				match Keymap::load(conn) {
					Ok(k) => keymap = k,
					Err(error) => debug!(%error, "keeping the old keymap"),
				}
				continue;
			}
			_ => continue,
		};
		if let Some(key) = key {
			lock().key(key, pressed);
		}
	}
}
