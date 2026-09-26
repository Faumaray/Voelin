//! X11 capture against a real X server: draws known windows, captures them
//! (MIT-SHM and GetImage), checks pixels, and sends a window capture through
//! VP8 and back.
//!
//! Needs `DISPLAY` (e.g. `Xvfb :101 -screen 0 1280x720x24 & DISPLAY=:101
//! cargo test -p voelin-media --test x11_capture`); skipped without it.
#![cfg(all(target_os = "linux", feature = "x11", feature = "vpx"))]

use std::time::Duration;

use voelin_media::capture::x11::X11Capture;
use voelin_media::{
	CaptureOptions, Codec, Codecs, EncoderConfig, FrameReceiver, ScreenCapture, SourceId,
	VideoFrame, convert,
};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
	AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const TITLE: &str = "voelin-media-x11-test";
const BACKGROUND: u32 = 0x202020;
const WINDOW: u32 = 0x3060c0;
const INNER: u32 = 0xf08020;
/// Test window: position and size on the root window.
const WX: i16 = 160;
const WY: i16 = 120;
const WW: u16 = 320;
const WH: u16 = 240;
/// Inner child window, relative to the test window.
const IX: i16 = 80;
const IY: i16 = 60;
const IW: u16 = 160;
const IH: u16 = 120;

fn rgb(pixel: u32) -> [u8; 3] {
	[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]
}

/// Windows drawn for the test, destroyed on drop.
struct Scene {
	conn: RustConnection,
	root: Window,
	windows: Vec<Window>,
	test_window: Window,
	screen: (u16, u16),
}

impl Scene {
	fn create(display: &str) -> Self {
		let (conn, screen_num) = RustConnection::connect(Some(display)).expect("connect");
		let screen = &conn.setup().roots[screen_num];
		let (root, depth, visual) = (screen.root, screen.root_depth, screen.root_visual);
		let size = (screen.width_in_pixels, screen.height_in_pixels);
		let mut windows = Vec::new();
		let mut create = |parent, x, y, w, h, pixel, override_redirect| {
			let id = conn.generate_id().unwrap();
			let aux = CreateWindowAux::new()
				.background_pixel(pixel)
				.override_redirect(u32::from(override_redirect));
			conn.create_window(
				depth,
				id,
				parent,
				x,
				y,
				w,
				h,
				0,
				WindowClass::INPUT_OUTPUT,
				visual,
				&aux,
			)
			.unwrap();
			windows.push(id);
			id
		};
		// A flat background over the whole screen (not a listed window),
		// then the test window with a child on top.
		let background = create(root, 0, 0, size.0, size.1, BACKGROUND, true);
		let test_window = create(root, WX, WY, WW, WH, WINDOW, false);
		let inner = create(test_window, IX, IY, IW, IH, INNER, false);
		conn.change_property8(
			PropMode::REPLACE,
			test_window,
			AtomEnum::WM_NAME,
			AtomEnum::STRING,
			TITLE.as_bytes(),
		)
		.unwrap();
		for w in [background, test_window, inner] {
			conn.map_window(w).unwrap();
		}
		conn.sync().unwrap();
		// Backgrounds are painted when the exposures are processed.
		std::thread::sleep(Duration::from_millis(200));
		Self { conn, root, windows, test_window, screen: size }
	}

	fn warp_pointer(&self, x: i16, y: i16) {
		self.conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x, y).unwrap();
		self.conn.sync().unwrap();
	}
}

impl Drop for Scene {
	fn drop(&mut self) {
		for &w in self.windows.iter().rev() {
			let _ = self.conn.destroy_window(w);
		}
		let _ = self.conn.flush();
	}
}

async fn first_frame(frames: &mut FrameReceiver<VideoFrame>) -> VideoFrame {
	tokio::time::timeout(Duration::from_secs(10), frames.recv())
		.await
		.expect("a frame within 10 s")
		.expect("capture ended")
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
	let i = ((y * width + x) * 4) as usize;
	[rgba[i], rgba[i + 1], rgba[i + 2]]
}

fn assert_close(actual: [u8; 3], expected: [u8; 3], tolerance: i32, what: &str) {
	let ok = (0..3).all(|c| (i32::from(actual[c]) - i32::from(expected[c])).abs() <= tolerance);
	assert!(ok, "{what}: got {actual:?}, expected {expected:?} ±{tolerance}");
}

/// Capture monitor 0 and check the drawn windows' pixels exactly.
async fn check_monitor(capture: &mut X11Capture, scene: &Scene) {
	let options = CaptureOptions { fps: 10, cursor: false, queue: 2 };
	let mut frames = capture.start(&SourceId::Monitor(0), &options).await.unwrap();
	let frame = first_frame(&mut frames).await;
	capture.stop();
	assert_eq!((frame.width, frame.height), (u32::from(scene.screen.0), u32::from(scene.screen.1)));
	let rgba = convert::to_rgba_vec(&frame).unwrap();
	let w = frame.width;
	assert_eq!(pixel(&rgba, w, 20, 20), rgb(BACKGROUND), "background");
	let (wx, wy) = (WX as u32, WY as u32);
	assert_eq!(pixel(&rgba, w, wx + 10, wy + 10), rgb(WINDOW), "test window");
	let inner = (wx + IX as u32 + 10, wy + IY as u32 + 10);
	assert_eq!(pixel(&rgba, w, inner.0, inner.1), rgb(INNER), "inner window");
	assert_eq!(pixel(&rgba, w, wx + WW as u32 + 5, wy), rgb(BACKGROUND), "right of the window");
}

#[tokio::test(flavor = "multi_thread")]
async fn x11_capture_end_to_end() {
	let Some(display) = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()) else {
		eprintln!("skipped: DISPLAY is not set");
		return;
	};
	let scene = Scene::create(&display);

	// Sources: the whole screen (or RandR monitors) and our named window.
	let mut capture = X11Capture::with_display(display.clone());
	let sources = capture.sources().unwrap();
	let monitor = sources.iter().find(|s| s.id == SourceId::Monitor(0)).expect("monitor 0");
	assert!(monitor.width > 0 && monitor.height > 0);
	let window = sources
		.iter()
		.find(|s| s.name == TITLE)
		.unwrap_or_else(|| panic!("test window not listed in {sources:?}"));
	assert_eq!(window.id, SourceId::Window(scene.test_window.into()));
	assert_eq!((window.width, window.height), (u32::from(WW), u32::from(WH)));

	// Same pixels through MIT-SHM and through GetImage.
	check_monitor(&mut capture, &scene).await;
	let mut get_image = X11Capture::with_display(display.clone()).use_shm(false);
	check_monitor(&mut get_image, &scene).await;

	// Unknown sources fail at start.
	let options = CaptureOptions { fps: 10, cursor: false, queue: 2 };
	assert!(capture.start(&SourceId::Monitor(99), &options).await.is_err());
	assert!(capture.start(&SourceId::Window(0x7fff_fff0), &options).await.is_err());

	// Window capture -> VP8 -> decode: the colours survive.
	let mut frames = capture.start(&window.id, &options).await.unwrap();
	let codecs = Codecs::new();
	let config = EncoderConfig { fps: 10, bitrate_bps: 1_000_000, ..EncoderConfig::default() };
	let mut encoder = codecs.new_encoder(Codec::Vp8, config).unwrap();
	let mut decoder = codecs.new_decoder(Codec::Vp8).unwrap();
	let mut decoded = None;
	let mut source = None;
	for i in 0..4 {
		let frame = first_frame(&mut frames).await;
		assert_eq!((frame.width, frame.height), (u32::from(WW), u32::from(WH)));
		let encoded = encoder.encode(&frame, false).unwrap();
		assert_eq!(encoded.len(), 1);
		assert_eq!(encoded[0].keyframe, i == 0);
		decoded = decoder.decode(&encoded[0].data).unwrap();
		source = Some(frame);
	}
	capture.stop();
	let (decoded, source) = (decoded.expect("decoded frame"), source.unwrap());
	let psnr = convert::psnr(&source, &decoded).unwrap();
	assert!(psnr > 30.0, "PSNR {psnr:.1} dB");
	let rgba = convert::to_rgba_vec(&decoded).unwrap();
	let w = decoded.width;
	assert_close(pixel(&rgba, w, 20, 20), rgb(WINDOW), 12, "decoded window corner");
	let center = (IX as u32 + IW as u32 / 2, IY as u32 + IH as u32 / 2);
	assert_close(pixel(&rgba, w, center.0, center.1), rgb(INNER), 12, "decoded inner window");

	// The cursor is drawn when asked for.
	scene.warp_pointer(40, 40);
	let with_cursor = CaptureOptions { fps: 10, cursor: true, queue: 2 };
	let mut frames = capture.start(&SourceId::Monitor(0), &with_cursor).await.unwrap();
	let frame = first_frame(&mut frames).await;
	capture.stop();
	let rgba = convert::to_rgba_vec(&frame).unwrap();
	let background = rgb(BACKGROUND);
	let changed = (24..72)
		.flat_map(|y| (24..72).map(move |x| (x, y)))
		.filter(|&(x, y)| pixel(&rgba, frame.width, x, y) != background)
		.count();
	assert!(changed > 10, "no cursor drawn near (40, 40): {changed} pixels differ");
}
