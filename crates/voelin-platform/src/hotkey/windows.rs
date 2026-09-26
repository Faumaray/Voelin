//! Windows backend: low-level keyboard and mouse hooks.
//!
//! `RegisterHotKey` only reports presses and takes the key away from other
//! applications, so push-to-talk uses `WH_KEYBOARD_LL` / `WH_MOUSE_LL`
//! instead. The hooks run on a dedicated thread with a message loop (Windows
//! calls low-level hooks on the installing thread) and always pass events
//! on. Hook procedures get no user data, so the thread's tracker lives in a
//! thread local.
//!
//! This is the crate's only unsafe code: the Win32 calls and reading the
//! hook structs Windows hands to the callbacks.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex, PoisonError, mpsc as std_mpsc};
use std::thread;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
	CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
	LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
	UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN,
	WM_MBUTTONUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};
use windows::core::PCWSTR;

use super::tracker::Tracker;
use super::{Hotkey, HotkeyEvents, Key, NamedKey};
use crate::{Error, Result};

thread_local! {
	/// The tracker of the hook thread this runs on.
	static TRACKER: RefCell<Option<Arc<Mutex<Tracker>>>> = const { RefCell::new(None) };
}

fn with_tracker(f: impl FnOnce(&mut Tracker)) {
	TRACKER.with(|t| {
		if let Some(tracker) = &*t.borrow() {
			f(&mut tracker.lock().unwrap_or_else(PoisonError::into_inner));
		}
	});
}

/// Keyboard hook procedure.
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
	if code == HC_ACTION as i32 {
		// SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a
		// KBDLLHOOKSTRUCT that is valid for the duration of this call.
		let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
		let pressed = match wparam.0 as u32 {
			WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
			WM_KEYUP | WM_SYSKEYUP => Some(false),
			_ => None,
		};
		let extended = info.flags.contains(LLKHF_EXTENDED);
		if let (Some(pressed), Some(key)) =
			(pressed, Key::from_windows_vk(info.vkCode as u16, extended))
		{
			with_tracker(|t| t.key(key, pressed));
		}
	}
	// SAFETY: passing the unchanged arguments on to the next hook.
	unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Mouse hook procedure: middle and side buttons.
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
	if code == HC_ACTION as i32 {
		// SAFETY: for WH_MOUSE_LL with HC_ACTION, lparam points to a
		// MSLLHOOKSTRUCT that is valid for the duration of this call.
		let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
		let side = |pressed| {
			// The high word of mouseData: 1 = XBUTTON1 (back), 2 = forward.
			let key = match info.mouseData >> 16 {
				1 => NamedKey::MouseBack,
				2 => NamedKey::MouseForward,
				_ => return None,
			};
			Some((Key::Named(key), pressed))
		};
		let event = match wparam.0 as u32 {
			WM_MBUTTONDOWN => Some((Key::Named(NamedKey::MouseMiddle), true)),
			WM_MBUTTONUP => Some((Key::Named(NamedKey::MouseMiddle), false)),
			WM_XBUTTONDOWN => side(true),
			WM_XBUTTONUP => side(false),
			_ => None,
		};
		if let Some((key, pressed)) = event {
			with_tracker(|t| t.key(key, pressed));
		}
	}
	// SAFETY: passing the unchanged arguments on to the next hook.
	unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Install both hooks on this thread.
fn install() -> windows::core::Result<(HHOOK, HHOOK)> {
	// SAFETY: plain Win32 calls with valid arguments; the hook procedures
	// have the HOOKPROC signature and live for the whole program.
	unsafe {
		let module = HINSTANCE::from(GetModuleHandleW(PCWSTR::null())?);
		let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(module), 0)?;
		match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(module), 0) {
			Ok(mouse) => Ok((keyboard, mouse)),
			Err(e) => {
				let _ = UnhookWindowsHookEx(keyboard);
				Err(e)
			}
		}
	}
}

fn hook_thread(tracker: Arc<Mutex<Tracker>>, ready: std_mpsc::Sender<Result<u32>>) {
	TRACKER.with(|t| *t.borrow_mut() = Some(tracker.clone()));
	let hooks = match install() {
		Ok(hooks) => hooks,
		Err(e) => {
			let _ = ready.send(Err(Error::Backend(format!("keyboard hook: {e}"))));
			return;
		}
	};
	// SAFETY: returns the id of the calling thread.
	let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
	let mut msg = MSG::default();
	// SAFETY: standard message loop on this thread's queue; `msg` is valid.
	// GetMessageW returns 0 for WM_QUIT and -1 on errors.
	unsafe {
		while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
			let _ = TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		let _ = UnhookWindowsHookEx(hooks.0);
		let _ = UnhookWindowsHookEx(hooks.1);
	}
	tracker.lock().unwrap_or_else(PoisonError::into_inner).release_all();
}

pub(crate) struct HookBackend {
	tracker: Arc<Mutex<Tracker>>,
	thread_id: u32,
}

impl HookBackend {
	pub fn start() -> Result<Self> {
		let tracker = Arc::new(Mutex::new(Tracker::default()));
		let (ready_tx, ready_rx) = std_mpsc::channel();
		{
			let tracker = tracker.clone();
			thread::Builder::new()
				.name("voelin-hotkeys-hook".into())
				.spawn(move || hook_thread(tracker, ready_tx))
				.map_err(|e| Error::Backend(format!("keyboard hook: {e}")))?;
		}
		let thread_id =
			ready_rx.recv().map_err(|_| Error::Backend("keyboard hook thread ended".into()))??;
		Ok(Self { tracker, thread_id })
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

impl Drop for HookBackend {
	fn drop(&mut self) {
		// SAFETY: posting WM_QUIT to our hook thread's queue ends its loop.
		let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
	}
}
