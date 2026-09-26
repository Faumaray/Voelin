//! Keys and key combinations, with their X11 keysyms, Windows virtual-key
//! codes and xdg "shortcuts" names.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

macro_rules! named_keys {
	($($variant:ident $name:literal $keysym:literal $vk:literal $xdg:literal;)*) => {
		/// Keys that are not letters, digits, function or keypad digit keys.
		#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
		pub enum NamedKey {
			$($variant,)*
		}

		impl NamedKey {
			pub const ALL: &[NamedKey] = &[$(NamedKey::$variant,)*];

			pub fn name(self) -> &'static str {
				match self {
					$(NamedKey::$variant => $name,)*
				}
			}

			fn keysym(self) -> u32 {
				match self {
					$(NamedKey::$variant => $keysym,)*
				}
			}

			fn vk(self) -> u16 {
				match self {
					$(NamedKey::$variant => $vk,)*
				}
			}

			fn xdg(self) -> &'static str {
				match self {
					$(NamedKey::$variant => $xdg,)*
				}
			}
		}
	};
}

// Mouse buttons have no keysym (0) and no xdg name (""); their codes are
// handled by the backends.
named_keys! {
	Space "Space" 0x0020 0x20 "space";
	Tab "Tab" 0xff09 0x09 "Tab";
	CapsLock "CapsLock" 0xffe5 0x14 "Caps_Lock";
	Backquote "Backquote" 0x0060 0xc0 "grave";
	Minus "Minus" 0x002d 0xbd "minus";
	Equal "Equal" 0x003d 0xbb "equal";
	BracketLeft "BracketLeft" 0x005b 0xdb "bracketleft";
	BracketRight "BracketRight" 0x005d 0xdd "bracketright";
	Backslash "Backslash" 0x005c 0xdc "backslash";
	Semicolon "Semicolon" 0x003b 0xba "semicolon";
	Quote "Quote" 0x0027 0xde "apostrophe";
	Comma "Comma" 0x002c 0xbc "comma";
	Period "Period" 0x002e 0xbe "period";
	Slash "Slash" 0x002f 0xbf "slash";
	Enter "Enter" 0xff0d 0x0d "Return";
	Escape "Escape" 0xff1b 0x1b "Escape";
	Backspace "Backspace" 0xff08 0x08 "BackSpace";
	Insert "Insert" 0xff63 0x2d "Insert";
	Delete "Delete" 0xffff 0x2e "Delete";
	Home "Home" 0xff50 0x24 "Home";
	End "End" 0xff57 0x23 "End";
	PageUp "PageUp" 0xff55 0x21 "Prior";
	PageDown "PageDown" 0xff56 0x22 "Next";
	Left "Left" 0xff51 0x25 "Left";
	Up "Up" 0xff52 0x26 "Up";
	Right "Right" 0xff53 0x27 "Right";
	Down "Down" 0xff54 0x28 "Down";
	Pause "Pause" 0xff13 0x13 "Pause";
	ScrollLock "ScrollLock" 0xff14 0x91 "Scroll_Lock";
	PrintScreen "PrintScreen" 0xff61 0x2c "Print";
	NumLock "NumLock" 0xff7f 0x90 "Num_Lock";
	NumpadAdd "NumpadAdd" 0xffab 0x6b "KP_Add";
	NumpadSubtract "NumpadSubtract" 0xffad 0x6d "KP_Subtract";
	NumpadMultiply "NumpadMultiply" 0xffaa 0x6a "KP_Multiply";
	NumpadDivide "NumpadDivide" 0xffaf 0x6f "KP_Divide";
	NumpadDecimal "NumpadDecimal" 0xffae 0x6e "KP_Decimal";
	// Windows reports it as Enter with the extended flag.
	NumpadEnter "NumpadEnter" 0xff8d 0x0d "KP_Enter";
	Menu "Menu" 0xff67 0x5d "Menu";
	LeftCtrl "LeftCtrl" 0xffe3 0xa2 "Control_L";
	RightCtrl "RightCtrl" 0xffe4 0xa3 "Control_R";
	LeftShift "LeftShift" 0xffe1 0xa0 "Shift_L";
	RightShift "RightShift" 0xffe2 0xa1 "Shift_R";
	LeftAlt "LeftAlt" 0xffe9 0xa4 "Alt_L";
	RightAlt "RightAlt" 0xffea 0xa5 "Alt_R";
	LeftSuper "LeftSuper" 0xffeb 0x5b "Super_L";
	RightSuper "RightSuper" 0xffec 0x5c "Super_R";
	MouseMiddle "MouseMiddle" 0 0x04 "";
	MouseBack "MouseBack" 0 0x05 "";
	MouseForward "MouseForward" 0 0x06 "";
}

/// X11 keysym of AltGr on most layouts, sitting on the right Alt key.
const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;

/// A physical key (or mouse side button) as a hotkey trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
	/// `'A'..='Z'`.
	Letter(char),
	/// 0 to 9 on the main keyboard.
	Digit(u8),
	/// F1 to F24.
	F(u8),
	/// 0 to 9 on the keypad.
	Numpad(u8),
	Named(NamedKey),
}

/// Which modifier a key is, if it is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModifierKind {
	Ctrl,
	Shift,
	Alt,
	Super,
}

impl Key {
	fn validate(self) -> Result<Self, Error> {
		let ok = match self {
			Key::Letter(c) => c.is_ascii_uppercase(),
			Key::Digit(d) | Key::Numpad(d) => d <= 9,
			Key::F(n) => (1..=24).contains(&n),
			Key::Named(_) => true,
		};
		if ok { Ok(self) } else { Err(Error::InvalidHotkey(format!("{self:?}"))) }
	}

	pub fn is_mouse(self) -> bool {
		matches!(
			self,
			Key::Named(NamedKey::MouseMiddle | NamedKey::MouseBack | NamedKey::MouseForward)
		)
	}

	#[cfg(any(test, windows, all(unix, not(target_os = "macos"), feature = "x11")))]
	pub(crate) fn modifier(self) -> Option<ModifierKind> {
		match self {
			Key::Named(NamedKey::LeftCtrl | NamedKey::RightCtrl) => Some(ModifierKind::Ctrl),
			Key::Named(NamedKey::LeftShift | NamedKey::RightShift) => Some(ModifierKind::Shift),
			Key::Named(NamedKey::LeftAlt | NamedKey::RightAlt) => Some(ModifierKind::Alt),
			Key::Named(NamedKey::LeftSuper | NamedKey::RightSuper) => Some(ModifierKind::Super),
			_ => None,
		}
	}

	/// X11 keysym (for letters the lowercase one). 0 for mouse buttons.
	pub fn x11_keysym(self) -> u32 {
		match self {
			Key::Letter(c) => c.to_ascii_lowercase() as u32,
			Key::Digit(d) => 0x30 + d as u32,
			Key::F(n) => 0xffbe + n as u32 - 1,
			Key::Numpad(d) => 0xffb0 + d as u32,
			Key::Named(k) => k.keysym(),
		}
	}

	/// The key an X11 keysym belongs to (any shift level).
	pub fn from_x11_keysym(keysym: u32) -> Option<Key> {
		match keysym {
			0x61..=0x7a => Some(Key::Letter((keysym as u8 - 0x20) as char)),
			0x41..=0x5a => Some(Key::Letter(keysym as u8 as char)),
			0x30..=0x39 => Some(Key::Digit((keysym - 0x30) as u8)),
			0xffbe..=0xffd5 => Some(Key::F((keysym - 0xffbe + 1) as u8)),
			0xffb0..=0xffb9 => Some(Key::Numpad((keysym - 0xffb0) as u8)),
			XK_ISO_LEVEL3_SHIFT => Some(Key::Named(NamedKey::RightAlt)),
			0 => None,
			_ => NamedKey::ALL.iter().find(|k| k.keysym() == keysym).map(|&k| Key::Named(k)),
		}
	}

	/// Windows virtual-key code. Mouse buttons use VK_MBUTTON/VK_XBUTTON1/2.
	pub fn windows_vk(self) -> u16 {
		match self {
			Key::Letter(c) => c as u16,
			Key::Digit(d) => 0x30 + d as u16,
			Key::F(n) => 0x70 + n as u16 - 1,
			Key::Numpad(d) => 0x60 + d as u16,
			Key::Named(k) => k.vk(),
		}
	}

	/// The key for a Windows virtual-key code; `extended` is the
	/// low-level hook's extended-key flag (tells keypad Enter apart).
	pub fn from_windows_vk(vk: u16, extended: bool) -> Option<Key> {
		match vk {
			0x0d if extended => Some(Key::Named(NamedKey::NumpadEnter)),
			0x41..=0x5a => Some(Key::Letter(vk as u8 as char)),
			0x30..=0x39 => Some(Key::Digit((vk - 0x30) as u8)),
			0x70..=0x87 => Some(Key::F((vk - 0x70 + 1) as u8)),
			0x60..=0x69 => Some(Key::Numpad((vk - 0x60) as u8)),
			// Generic Ctrl/Shift/Alt (never sent to low-level hooks, but cheap).
			0x10 => Some(Key::Named(NamedKey::LeftShift)),
			0x11 => Some(Key::Named(NamedKey::LeftCtrl)),
			0x12 => Some(Key::Named(NamedKey::LeftAlt)),
			_ => NamedKey::ALL
				.iter()
				.find(|k| k.vk() == vk && **k != NamedKey::NumpadEnter)
				.map(|&k| Key::Named(k)),
		}
	}

	/// Keysym name as used by the xdg "shortcuts" specification (portal
	/// preferred triggers). `None` for mouse buttons.
	pub fn xdg_name(self) -> Option<String> {
		let name = match self {
			Key::Letter(c) => c.to_ascii_lowercase().to_string(),
			Key::Digit(d) => d.to_string(),
			Key::F(n) => format!("F{n}"),
			Key::Numpad(d) => format!("KP_{d}"),
			Key::Named(k) => k.xdg().to_string(),
		};
		(!name.is_empty()).then_some(name)
	}
}

impl fmt::Display for Key {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Key::Letter(c) => write!(f, "{c}"),
			Key::Digit(d) => write!(f, "{d}"),
			Key::F(n) => write!(f, "F{n}"),
			Key::Numpad(d) => write!(f, "Numpad{d}"),
			Key::Named(k) => f.write_str(k.name()),
		}
	}
}

impl FromStr for Key {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self, Error> {
		let invalid = || Error::InvalidHotkey(s.to_string());
		let key = if s.len() == 1 {
			let c = s.chars().next().ok_or_else(invalid)?;
			match c {
				'a'..='z' | 'A'..='Z' => Key::Letter(c.to_ascii_uppercase()),
				'0'..='9' => Key::Digit(c as u8 - b'0'),
				_ => return Err(invalid()),
			}
		} else if let Some(d) = s.strip_prefix("Numpad").and_then(|d| d.parse().ok()) {
			Key::Numpad(d)
		} else if let Some(n) = s.strip_prefix(['F', 'f']).and_then(|n| n.parse().ok()) {
			Key::F(n)
		} else {
			let named = NamedKey::ALL.iter().find(|k| k.name().eq_ignore_ascii_case(s));
			Key::Named(*named.ok_or_else(invalid)?)
		};
		key.validate().map_err(|_| invalid())
	}
}

/// Modifiers that must be held with a hotkey's key. Either side counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
	pub ctrl: bool,
	pub shift: bool,
	pub alt: bool,
	/// The Windows / Command / "logo" key.
	pub logo: bool,
}

impl Modifiers {
	pub const NONE: Modifiers = Modifiers { ctrl: false, shift: false, alt: false, logo: false };

	/// Every modifier in `self` is also in `held`.
	pub fn is_subset_of(self, held: Modifiers) -> bool {
		(!self.ctrl || held.ctrl)
			&& (!self.shift || held.shift)
			&& (!self.alt || held.alt)
			&& (!self.logo || held.logo)
	}

	pub(crate) fn add(&mut self, kind: ModifierKind) {
		match kind {
			ModifierKind::Ctrl => self.ctrl = true,
			ModifierKind::Shift => self.shift = true,
			ModifierKind::Alt => self.alt = true,
			ModifierKind::Super => self.logo = true,
		}
	}

	fn names(self) -> impl Iterator<Item = (&'static str, &'static str)> {
		[
			(self.ctrl, "Ctrl", "CTRL"),
			(self.shift, "Shift", "SHIFT"),
			(self.alt, "Alt", "ALT"),
			(self.logo, "Super", "LOGO"),
		]
		.into_iter()
		.filter(|(on, ..)| *on)
		.map(|(_, name, xdg)| (name, xdg))
	}
}

/// A key plus required modifiers, e.g. `Ctrl+Shift+T`, `F13`, `RightCtrl`
/// or `MouseBack`.
///
/// Fires when the key goes down while at least these modifiers are held
/// (extra modifiers do not prevent it, so push-to-talk keeps working while
/// typing with Shift), and releases when the key goes up. Serialized as
/// that string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hotkey {
	pub modifiers: Modifiers,
	pub key: Key,
}

impl Hotkey {
	pub fn new(key: Key) -> Self {
		Self { modifiers: Modifiers::NONE, key }
	}

	pub fn with(mut self, modifiers: Modifiers) -> Self {
		self.modifiers = modifiers;
		self
	}

	/// Trigger in the xdg "shortcuts" format (`CTRL+SHIFT+t`), as the
	/// portal's preferred trigger. `None` for mouse buttons.
	pub fn xdg_trigger(&self) -> Option<String> {
		let key = self.key.xdg_name()?;
		let mut parts: Vec<String> = self.modifiers.names().map(|(_, x)| x.to_string()).collect();
		parts.push(key);
		Some(parts.join("+"))
	}
}

impl fmt::Display for Hotkey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		for (name, _) in self.modifiers.names() {
			write!(f, "{name}+")?;
		}
		write!(f, "{}", self.key)
	}
}

impl FromStr for Hotkey {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self, Error> {
		let mut parts: Vec<&str> = s.split('+').map(str::trim).collect();
		// "Ctrl++" is not supported; Plus is not a key here.
		let key = parts.pop().filter(|k| !k.is_empty());
		let key: Key = key.ok_or_else(|| Error::InvalidHotkey(s.to_string()))?.parse()?;
		let mut modifiers = Modifiers::NONE;
		for part in parts {
			let kind = match part.to_ascii_lowercase().as_str() {
				"ctrl" | "control" => ModifierKind::Ctrl,
				"shift" => ModifierKind::Shift,
				"alt" => ModifierKind::Alt,
				"super" | "logo" | "win" | "meta" => ModifierKind::Super,
				_ => return Err(Error::InvalidHotkey(s.to_string())),
			};
			modifiers.add(kind);
		}
		Ok(Hotkey { modifiers, key })
	}
}

impl TryFrom<String> for Hotkey {
	type Error = Error;

	fn try_from(s: String) -> Result<Self, Error> {
		s.parse()
	}
}

impl From<Hotkey> for String {
	fn from(h: Hotkey) -> String {
		h.to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_and_display() {
		for s in
			["Ctrl+Shift+T", "F13", "RightCtrl", "MouseBack", "Alt+Numpad5", "Super+Space", "7"]
		{
			let h: Hotkey = s.parse().unwrap();
			assert_eq!(h.to_string(), s);
		}
		let h: Hotkey = "control + shift + t".parse().unwrap();
		assert_eq!(h.to_string(), "Ctrl+Shift+T");
		assert_eq!("win+f1".parse::<Hotkey>().unwrap().to_string(), "Super+F1");
		for bad in ["", "Ctrl+", "Hyper+A", "F25", "F0", "Numpad10", "NoSuchKey", "Ctrl+Shift"] {
			assert!(bad.parse::<Hotkey>().is_err(), "{bad} should not parse");
		}
	}

	#[test]
	fn serde_as_string() {
		let h: Hotkey = "Ctrl+F13".parse().unwrap();
		let json = serde_json::to_string(&h).unwrap();
		assert_eq!(json, r#""Ctrl+F13""#);
		assert_eq!(serde_json::from_str::<Hotkey>(&json).unwrap(), h);
		assert!(serde_json::from_str::<Hotkey>(r#""Ctrl+Nope""#).is_err());
	}

	#[test]
	fn platform_codes_roundtrip() {
		let mut keys: Vec<Key> = NamedKey::ALL.iter().map(|&k| Key::Named(k)).collect();
		keys.extend(('A'..='Z').map(Key::Letter));
		keys.extend((0..=9).map(Key::Digit));
		keys.extend((0..=9).map(Key::Numpad));
		keys.extend((1..=24).map(Key::F));
		for key in keys {
			let extended = key == Key::Named(NamedKey::NumpadEnter);
			assert_eq!(Key::from_windows_vk(key.windows_vk(), extended), Some(key), "{key}");
			if !key.is_mouse() {
				assert_eq!(Key::from_x11_keysym(key.x11_keysym()), Some(key), "{key}");
				assert!(key.xdg_name().is_some());
			}
			assert_eq!(key.to_string().parse::<Key>().unwrap(), key);
		}
		// Shifted letters and AltGr.
		assert_eq!(Key::from_x11_keysym(0x41), Some(Key::Letter('A')));
		assert_eq!(Key::from_x11_keysym(0xfe03), Some(Key::Named(NamedKey::RightAlt)));
		assert_eq!(Key::from_x11_keysym(0x1234), None);
	}

	#[test]
	fn xdg_triggers() {
		let h: Hotkey = "Ctrl+Shift+T".parse().unwrap();
		assert_eq!(h.xdg_trigger().as_deref(), Some("CTRL+SHIFT+t"));
		let h: Hotkey = "Super+PageUp".parse().unwrap();
		assert_eq!(h.xdg_trigger().as_deref(), Some("LOGO+Prior"));
		assert_eq!("MouseBack".parse::<Hotkey>().unwrap().xdg_trigger(), None);
	}

	#[test]
	fn modifier_subsets() {
		let ctrl = Modifiers { ctrl: true, ..Modifiers::NONE };
		let ctrl_shift = Modifiers { shift: true, ..ctrl };
		assert!(Modifiers::NONE.is_subset_of(ctrl));
		assert!(ctrl.is_subset_of(ctrl_shift));
		assert!(!ctrl_shift.is_subset_of(ctrl));
	}
}
