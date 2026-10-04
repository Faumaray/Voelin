//! A connection that bytes are written to through libavformat's protocols
//! (`rtmp://`, `rtmps://`, and whatever else the installed FFmpeg speaks).
//!
//! `avio_write` buffers and `avio_flush` sends; neither returns an error,
//! which lands in `AVIOContext.error` instead (found at load,
//! [`super::layout::avio_error`]), so every write and flush is checked
//! there. Every blocking call (connecting, sending, closing) polls an
//! interrupt callback, so a stuck network or a stop from another thread
//! never holds the caller for longer than FFmpeg's poll interval (100 ms)
//! once the abort flag is set.
#![allow(unsafe_code)]

use std::ffi::c_int;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::layout::read;
use super::sys::{AVIO_FLAG_WRITE, InterruptCallback, Ptr, cstr};
use super::{Ffmpeg, Io, take_log};

/// An open connection; closed (cleanly, as far as the abort flag lets it)
/// when dropped.
pub struct Connection {
	ffmpeg: &'static Ffmpeg,
	ctx: Ptr,
	/// What the interrupt callback reads; kept alive as long as the context.
	abort: Arc<AtomicBool>,
}

// SAFETY: the context is only used through `&mut self` or by value, one
// thread at a time; FFmpeg's I/O contexts have no thread affinity.
unsafe impl Send for Connection {}

unsafe extern "C" fn interrupted(opaque: Ptr) -> c_int {
	// SAFETY: `opaque` is the `AtomicBool` of the `Arc` the connection
	// holds for as long as FFmpeg may call this (until its context is
	// closed).
	c_int::from(unsafe { (*opaque.cast::<AtomicBool>()).load(Ordering::Relaxed) })
}

/// The FFmpeg messages about the connection since the last call, for an
/// error text.
fn messages() -> String {
	let mut lines = Vec::new();
	for context in ["rtmp", "tls", "tcp"] {
		lines.extend(take_log(context));
	}
	lines.join("; ")
}

impl Connection {
	/// Whether this FFmpeg can open connections: libavformat's I/O was
	/// loaded and its write errors can be read.
	pub fn available() -> Result<(), String> {
		Self::libraries().map(|_| ())
	}

	/// The process's FFmpeg and its I/O functions.
	fn libraries() -> Result<(&'static Ffmpeg, &'static Io), String> {
		let ffmpeg = Ffmpeg::get()?;
		Ok((ffmpeg, ffmpeg.io.as_ref().map_err(Clone::clone)?))
	}

	/// Open `url` for writing, with protocol `options` (`rw_timeout`,
	/// `rtmp_app`, `rtmp_playpath`, ...; options the protocol does not
	/// know are ignored). Once `abort` is set, every blocking call of this
	/// connection returns at once (with an error).
	pub fn open(
		url: &str,
		options: &[(&str, &str)],
		abort: Arc<AtomicBool>,
	) -> Result<Self, String> {
		let (ffmpeg, io) = Self::libraries()?;
		static NETWORK: OnceLock<()> = OnceLock::new();
		NETWORK.get_or_init(|| {
			// SAFETY: no arguments; initialises the TLS libraries once.
			unsafe { (io.api.avformat_network_init)() };
		});
		// Messages of an earlier connection are not about this one.
		messages();
		let url = cstr(url);
		let callback = InterruptCallback {
			callback: Some(interrupted),
			opaque: Arc::as_ptr(&abort).cast_mut().cast(),
		};
		let mut ctx: Ptr = std::ptr::null_mut();
		// SAFETY: a dictionary built with av_dict_set and freed with
		// av_dict_free; avio_open2 copies the callback and takes C strings
		// that outlive the call.
		let ret = unsafe {
			let mut dict: Ptr = std::ptr::null_mut();
			for (key, value) in options {
				let (key, value) = (cstr(key), cstr(value));
				(ffmpeg.api.av_dict_set)(&mut dict, key.as_ptr(), value.as_ptr(), 0);
			}
			let ret =
				(io.api.avio_open2)(&mut ctx, url.as_ptr(), AVIO_FLAG_WRITE, &callback, &mut dict);
			(ffmpeg.api.av_dict_free)(&mut dict);
			ret
		};
		if ret < 0 || ctx.is_null() {
			return Err(failure(ffmpeg, ret));
		}
		Ok(Self { ffmpeg, ctx, abort })
	}

	fn io(&self) -> &'static Io {
		// Checked in `open`.
		self.ffmpeg.io.as_ref().expect("a connection is only opened with libavformat's I/O")
	}

	/// The write error of the context, if one happened.
	fn check(&self) -> Result<(), String> {
		// SAFETY: `error` is `AVIOContext.error` of this release (located and
		// checked at load); the context is live.
		let error: c_int = unsafe { read(self.ctx, self.io().error) };
		if error < 0 { Err(failure(self.ffmpeg, error)) } else { Ok(()) }
	}

	/// Hand `data` to the connection (sent at the latest on [`flush`]).
	///
	/// [`flush`]: Self::flush
	pub fn write(&mut self, data: &[u8]) -> Result<(), String> {
		for chunk in data.chunks(c_int::MAX as usize) {
			// SAFETY: a live context; the slice is valid for its length.
			unsafe { (self.io().api.avio_write)(self.ctx, chunk.as_ptr(), chunk.len() as c_int) };
		}
		self.check()
	}

	/// Send everything written so far.
	pub fn flush(&mut self) -> Result<(), String> {
		// SAFETY: a live context.
		unsafe { (self.io().api.avio_flush)(self.ctx) };
		self.check()
	}

	/// Close the connection (for RTMP: unpublish), reporting a write that
	/// failed on the way.
	pub fn close(mut self) -> Result<(), String> {
		let error = self.check();
		let ret = self.close_context();
		error?;
		if ret < 0 { Err(failure(self.ffmpeg, ret)) } else { Ok(()) }
	}

	fn close_context(&mut self) -> c_int {
		if self.ctx.is_null() {
			return 0;
		}
		// SAFETY: a context from avio_open2; avio_closep sets it to NULL.
		unsafe { (self.io().api.avio_closep)(&mut self.ctx) }
	}

	/// Whether the abort flag is set.
	pub fn aborted(&self) -> bool {
		self.abort.load(Ordering::Relaxed)
	}
}

impl Drop for Connection {
	fn drop(&mut self) {
		self.close_context();
	}
}

/// FFmpeg's text for `code` with what it logged about the connection.
fn failure(ffmpeg: &Ffmpeg, code: c_int) -> String {
	let text = ffmpeg.api.error_text(code);
	match messages() {
		log if log.is_empty() => text,
		log => format!("{log} ({text})"),
	}
}

#[cfg(test)]
mod tests {
	use std::io::Read;
	use std::net::TcpListener;
	use std::time::{Duration, Instant};

	use super::*;

	/// A file written through libavformat's `file:` protocol arrives whole,
	/// and a write error (a connection the other side closed) is seen.
	#[test]
	fn writes_arrive_and_errors_are_seen() {
		if Connection::available().is_err() {
			eprintln!("no FFmpeg with libavformat, skipped");
			return;
		}
		let path = std::env::temp_dir().join(format!("voelin-avio-{}.bin", std::process::id()));
		let abort = Arc::new(AtomicBool::new(false));
		let mut file =
			Connection::open(&path.to_string_lossy(), &[], abort.clone()).expect("a file opens");
		file.write(b"hello ").unwrap();
		file.write(&[7; 100_000]).unwrap();
		file.flush().unwrap();
		file.close().unwrap();
		let written = std::fs::read(&path).unwrap();
		std::fs::remove_file(&path).unwrap();
		assert_eq!(written.len(), 100_006);
		assert!(written.starts_with(b"hello "));

		// A TCP peer that reads a little and goes away: the writes start
		// failing, and the failure is reported.
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		let peer = std::thread::spawn(move || {
			let (mut socket, _) = listener.accept().unwrap();
			let mut buf = [0; 16];
			let _ = socket.read_exact(&mut buf);
		});
		let mut tcp = Connection::open(
			&format!("tcp://127.0.0.1:{port}"),
			&[("rw_timeout", "2000000")],
			abort,
		)
		.unwrap();
		tcp.write(&[1; 16]).unwrap();
		tcp.flush().unwrap();
		peer.join().unwrap();
		let started = Instant::now();
		let error = loop {
			match tcp.write(&[1; 65536]).and_then(|()| tcp.flush()) {
				Ok(()) => assert!(started.elapsed() < Duration::from_secs(10), "no error"),
				Err(e) => break e,
			}
		};
		assert!(!error.is_empty());
		assert!(tcp.close().is_err());
	}

	/// An abort flag set while connecting makes the connection give up.
	#[test]
	fn an_abort_stops_a_connection_attempt() {
		if Connection::available().is_err() {
			return;
		}
		// A listener that never accepts: the RTMP handshake waits forever.
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		let abort = Arc::new(AtomicBool::new(false));
		let stopper = abort.clone();
		std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(300));
			stopper.store(true, Ordering::Relaxed);
		});
		let started = Instant::now();
		let result = Connection::open(&format!("rtmp://127.0.0.1:{port}/app/key"), &[], abort);
		assert!(result.is_err());
		assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
		drop(listener);
	}
}
