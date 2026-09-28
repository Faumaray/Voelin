//! wlroots capture against a real compositor: lists outputs, captures the
//! background colour, changes it through sway's IPC and sees the change.
//!
//! Needs a headless sway and its sockets, e.g.
//!
//! ```sh
//! printf 'output HEADLESS-1 resolution 640x480 bg #204080 solid_color\n' > /tmp/sway.conf
//! XDG_RUNTIME_DIR=/tmp/vsw WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
//!     WLR_RENDERER=pixman sway -c /tmp/sway.conf &
//! VOELIN_WLROOTS_TEST_DISPLAY=/tmp/vsw/wayland-1 \
//! VOELIN_WLROOTS_TEST_SWAYSOCK=$(ls /tmp/vsw/sway-ipc.*.sock) \
//!     cargo test -p voelin-media --test wlroots_capture
//! ```
//!
//! Skipped without `VOELIN_WLROOTS_TEST_DISPLAY`.
#![cfg(all(target_os = "linux", feature = "wlroots"))]

use std::sync::mpsc;
use std::time::Duration;

use voelin_media::capture::wlroots::WlrootsCapture;
use voelin_media::capture::{FrameSink, ScreenCapture};
use voelin_media::{CaptureOptions, FrameRef, PixelsRef, SourceId};

/// Sends the centre pixel (RGB) and size of each frame.
struct Probe(mpsc::Sender<([u8; 3], u32, u32)>);

impl FrameSink for Probe {
	fn max_fps(&self) -> u32 {
		30
	}

	fn frame(&mut self, frame: FrameRef<'_>) -> bool {
		let (x, y) = (frame.width as usize / 2, frame.height as usize / 2);
		let rgb = match frame.pixels {
			PixelsRef::Bgra(p) => {
				let px = &p.row(y, frame.width as usize * 4)[x * 4..x * 4 + 4];
				[px[2], px[1], px[0]]
			}
			PixelsRef::Rgba(p) => {
				let px = &p.row(y, frame.width as usize * 4)[x * 4..x * 4 + 4];
				[px[0], px[1], px[2]]
			}
			_ => panic!("unexpected format {:?}", frame.format()),
		};
		self.0.send((rgb, frame.width, frame.height)).is_ok()
	}
}

fn sway_background(color: &str) {
	let Some(sock) = std::env::var_os("VOELIN_WLROOTS_TEST_SWAYSOCK") else { return };
	let status = std::process::Command::new("swaymsg")
		.env("SWAYSOCK", sock)
		.args(["output", "*", "bg", color, "solid_color"])
		.status()
		.expect("swaymsg");
	assert!(status.success());
}

#[tokio::test]
async fn captures_outputs() {
	let Some(display) = std::env::var_os("VOELIN_WLROOTS_TEST_DISPLAY") else {
		eprintln!("skipped: VOELIN_WLROOTS_TEST_DISPLAY is not set");
		return;
	};
	sway_background("#204080");
	let mut capture = WlrootsCapture::with_display(&display);
	assert!(capture.is_available());
	let sources = capture.sources().unwrap();
	assert!(!sources.is_empty(), "no outputs");
	eprintln!("outputs: {sources:?}");
	assert_eq!(sources[0].id, SourceId::Monitor(0));
	assert!(capture.start(&SourceId::Monitor(99), &CaptureOptions::default()).await.is_err());

	let (tx, rx) = mpsc::channel();
	let options = CaptureOptions { cursor: false, ..CaptureOptions::default() };
	capture.start_sink(&SourceId::Monitor(0), &options, Box::new(Probe(tx))).await.unwrap();
	let (rgb, w, h) = rx.recv_timeout(Duration::from_secs(5)).expect("a first frame");
	assert_eq!((w, h), (sources[0].width, sources[0].height));
	assert_eq!(rgb, [0x20, 0x40, 0x80]);
	if std::env::var_os("VOELIN_WLROOTS_TEST_SWAYSOCK").is_some() {
		// New content: a new frame with the new colour.
		sway_background("#c04020");
		let changed = (0..20).any(|_| {
			rx.recv_timeout(Duration::from_secs(5)).is_ok_and(|(rgb, ..)| rgb == [0xc0, 0x40, 0x20])
		});
		assert!(changed, "the background change was not captured");
	}
	capture.stop();

	// The queue API copies frames out.
	let mut frames = capture.start(&SourceId::Monitor(0), &options).await.unwrap();
	let frame = frames.recv_timeout(Duration::from_secs(5)).expect("a queued frame");
	assert_eq!((frame.width, frame.height), (w, h));
	capture.stop();
	sway_background("#204080");
}
