//! Turns raw key presses and releases into hotkey events. Used by the
//! backends that see every key (X11 raw events, the Windows keyboard hook);
//! the portal does its own matching.

use std::collections::HashSet;

use tokio::sync::mpsc;

use super::{Hotkey, HotkeyEvent, HotkeyEvents, Key, Modifiers};

struct Registration {
	id: String,
	hotkey: Hotkey,
	events: mpsc::UnboundedSender<HotkeyEvent>,
	active: bool,
}

#[derive(Default)]
pub(crate) struct Tracker {
	registrations: Vec<Registration>,
	held: HashSet<Key>,
}

impl Tracker {
	/// Register (or replace) `id`.
	pub fn add(&mut self, id: &str, hotkey: Hotkey) -> HotkeyEvents {
		self.remove(id);
		let (tx, rx) = mpsc::unbounded_channel();
		self.registrations.push(Registration { id: id.into(), hotkey, events: tx, active: false });
		rx
	}

	pub fn remove(&mut self, id: &str) -> bool {
		let before = self.registrations.len();
		self.registrations.retain(|r| r.id != id);
		self.registrations.len() != before
	}

	#[cfg(test)]
	pub fn is_empty(&self) -> bool {
		self.registrations.is_empty()
	}

	fn held_modifiers(&self) -> Modifiers {
		let mut m = Modifiers::NONE;
		for kind in self.held.iter().filter_map(|k| k.modifier()) {
			m.add(kind);
		}
		m
	}

	/// A key went down (`pressed`) or up. Auto-repeat presses are ignored.
	pub fn key(&mut self, key: Key, pressed: bool) {
		if pressed {
			self.held.insert(key);
			let held = self.held_modifiers();
			for r in &mut self.registrations {
				if !r.active && r.hotkey.key == key && r.hotkey.modifiers.is_subset_of(held) {
					r.active = true;
					let _ = r.events.send(HotkeyEvent::Pressed);
				}
			}
		} else {
			self.held.remove(&key);
			for r in &mut self.registrations {
				if r.active && r.hotkey.key == key {
					r.active = false;
					let _ = r.events.send(HotkeyEvent::Released);
				}
			}
		}
		// Receivers that were dropped unregister themselves.
		self.registrations.retain(|r| !r.events.is_closed());
	}

	/// Release everything (e.g. the keyboard state was lost).
	pub fn release_all(&mut self) {
		self.held.clear();
		for r in &mut self.registrations {
			if r.active {
				r.active = false;
				let _ = r.events.send(HotkeyEvent::Released);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::super::NamedKey;
	use super::*;

	fn drain(rx: &mut HotkeyEvents) -> Vec<HotkeyEvent> {
		std::iter::from_fn(|| rx.try_recv().ok()).collect()
	}

	#[test]
	fn press_release_with_modifiers() {
		use HotkeyEvent::*;
		let mut t = Tracker::default();
		let mut ptt = t.add("ptt", "Ctrl+F13".parse().unwrap());
		let mut plain = t.add("mute", "M".parse().unwrap());
		let ctrl = Key::Named(NamedKey::RightCtrl);

		// Without Ctrl nothing happens for Ctrl+F13.
		t.key(Key::F(13), true);
		t.key(Key::F(13), false);
		assert!(drain(&mut ptt).is_empty());

		t.key(ctrl, true);
		t.key(Key::F(13), true);
		t.key(Key::F(13), true); // auto-repeat
		// Letting go of Ctrl first keeps it pressed until F13 is released.
		t.key(ctrl, false);
		assert_eq!(drain(&mut ptt), vec![Pressed]);
		t.key(Key::F(13), false);
		assert_eq!(drain(&mut ptt), vec![Released]);

		// Extra modifiers do not block a plain hotkey.
		t.key(Key::Named(NamedKey::LeftShift), true);
		t.key(Key::Letter('M'), true);
		t.key(Key::Letter('M'), false);
		assert_eq!(drain(&mut plain), vec![Pressed, Released]);
	}

	#[test]
	fn modifier_alone_and_unregister() {
		use HotkeyEvent::*;
		let mut t = Tracker::default();
		let mut rx = t.add("ptt", "RightCtrl".parse().unwrap());
		let key = Key::Named(NamedKey::RightCtrl);
		t.key(key, true);
		t.release_all();
		assert_eq!(drain(&mut rx), vec![Pressed, Released]);

		assert!(t.remove("ptt"));
		assert!(!t.remove("ptt"));
		t.key(key, true);
		assert!(rx.try_recv().is_err());

		// A dropped receiver unregisters on the next key.
		let rx = t.add("x", "A".parse().unwrap());
		drop(rx);
		t.key(Key::Letter('B'), true);
		assert!(t.is_empty());
	}
}
