//! The studio compositor allocates nothing per composed frame: several
//! sources of different sizes, scaled, cropped, blended and mirrored into a
//! 1080p canvas, counted with a global allocator that counts only on the
//! thread that asks.
//!
//! A scene change, a source that resizes and a new output size each replan
//! (and allocate); a steady composite must not.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use voelin_media::frame::{FrameData, Plane, VideoFrame};
use voelin_media::studio::compose::{Compositor, Feed, RgbaScaler};
use voelin_media::studio::scene::{Colour, Crop, Fit, Scene, Source, SourceKind, Transform};

thread_local! {
	static COUNTING: Cell<bool> = const { Cell::new(false) };
	static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn count() {
	// `try_with`: never panics, also while the thread is torn down; const
	// thread locals without destructors never allocate.
	let _ = COUNTING.try_with(|on| {
		if on.get() {
			let _ = COUNT.try_with(|n| n.set(n.get() + 1));
		}
	});
}

struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees hold; counting touches only
// thread-local cells and never allocates.
unsafe impl GlobalAlloc for Counting {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		count();
		// SAFETY: forwarded unchanged (see above).
		unsafe { System.alloc(layout) }
	}

	unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
		count();
		// SAFETY: forwarded unchanged.
		unsafe { System.alloc_zeroed(layout) }
	}

	unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
		count();
		// SAFETY: forwarded unchanged.
		unsafe { System.realloc(ptr, layout, new_size) }
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		// SAFETY: forwarded unchanged.
		unsafe { System.dealloc(ptr, layout) }
	}
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` makes on this thread.
fn allocations(f: impl FnOnce()) -> u64 {
	COUNT.with(|n| n.set(0));
	COUNTING.with(|on| on.set(true));
	f();
	COUNTING.with(|on| on.set(false));
	COUNT.with(Cell::get)
}

/// A `width` x `height` frame whose pixel `(x, y)` is a cheap pattern, RGBA or
/// BGRA.
fn frame(width: u32, height: u32, bgra: bool, seed: u8) -> Arc<VideoFrame> {
	let mut data = Vec::with_capacity(width as usize * height as usize * 4);
	for y in 0..height {
		for x in 0..width {
			let v = (x ^ y) as u8 ^ seed;
			data.extend_from_slice(&[v, v.wrapping_mul(3), seed, 128 | seed]);
		}
	}
	let plane = Plane::new(data, width as usize * 4);
	Arc::new(VideoFrame {
		width,
		height,
		timestamp: Duration::ZERO,
		data: if bgra { FrameData::Bgra(plane) } else { FrameData::Rgba(plane) },
	})
}

#[test]
fn composing_allocates_nothing_per_frame() {
	// The counter works.
	assert_eq!(allocations(|| drop(std::hint::black_box(vec![0u8; 16]))), 1);

	let mut scene = Scene::new(1, "alloc");
	scene.background = Colour::rgb(20, 20, 24);
	// A 4K screen scaled down to the canvas.
	scene.sources.push(Source {
		transform: Transform { fit: Fit::Cover, ..Transform::full(1920, 1080) },
		..Source::new(1, SourceKind::Screen { monitor: 0, backend: None, cursor: true })
	});
	// A camera, cropped, mirrored, in the corner.
	scene.sources.push(Source {
		transform: Transform::box_at(1520.0, 800.0, 384.0, 216.0),
		crop: Crop { left: 40, top: 0, right: 40, bottom: 0 },
		..Source::new(
			2,
			SourceKind::Camera { device: String::new(), size: None, fps: None, mirror: true },
		)
	});
	// A logo with alpha, at 1:1.
	scene.sources.push(Source {
		transform: Transform::box_at(32.0, 32.0, 128.0, 128.0),
		opacity: 0.75,
		..Source::new(3, SourceKind::Image { path: "logo.png".into() })
	});
	// Text, scaled up.
	scene.sources.push(Source {
		transform: Transform::box_at(32.0, 960.0, 600.0, 80.0),
		..Source::new(
			4,
			SourceKind::Text {
				text: "LIVE".into(),
				font: None,
				size_px: 48.0,
				colour: Colour::WHITE,
				backdrop: Colour::CLEAR,
				align: Default::default(),
				padding: 0,
			},
		)
	});
	let sizes = [(3840u32, 2160u32), (1280, 720), (128, 128), (600, 80)];
	let bgra = [true, true, false, false];
	let feeds: Vec<Option<Arc<Feed>>> =
		(0..scene.sources.len()).map(|_| Some(Arc::new(Feed::new()))).collect();
	let frames: Vec<Arc<VideoFrame>> = sizes
		.iter()
		.zip(bgra)
		.enumerate()
		.map(|(i, (&(w, h), bgra))| frame(w, h, bgra, i as u8 * 40))
		.collect();

	let mut compositor = Compositor::new(4);
	compositor.set_size(1920, 1080);
	let mut preview = RgbaScaler::new(2);
	// Every source delivers every frame except the camera (every third).
	let step = |compositor: &mut Compositor, preview: &mut RgbaScaler, n: u64| {
		for (i, feed) in feeds.iter().enumerate() {
			if i != 1 || n.is_multiple_of(3) {
				feed.as_ref().unwrap().put(frames[i].clone());
			}
		}
		let out =
			compositor.compose(&scene, 1, &feeds, Duration::from_millis(n * 16)).expect("composed");
		// The preview tap runs on the same thread in this test.
		drop(preview.scale(&out, 480, 270).expect("previewed"));
		drop(out);
	};
	// Warm up: the plan, the filter weights, the canvas pool and the scratch
	// rows are all in place after the first frames.
	for n in 0..12 {
		step(&mut compositor, &mut preview, n);
	}
	let count = allocations(|| {
		for n in 12..112 {
			step(&mut compositor, &mut preview, n);
		}
	});
	assert_eq!(count, 0, "allocations in 100 composed frames");
	let stats = compositor.stats();
	assert_eq!(stats.frames, 112);
	assert_eq!(stats.replans, 1, "the plan was made once");
	// One canvas: the previous one is released before the next compose.
	assert_eq!(stats.allocated, 1);
	assert!(stats.compose_time > Duration::ZERO);

	// A new output size replans, then allocates nothing again.
	compositor.set_size(1280, 720);
	for n in 112..124 {
		step(&mut compositor, &mut preview, n);
	}
	assert_eq!(compositor.stats().replans, 2);
	let count = allocations(|| {
		for n in 124..164 {
			step(&mut compositor, &mut preview, n);
		}
	});
	assert_eq!(count, 0, "allocations after a size change");
}
