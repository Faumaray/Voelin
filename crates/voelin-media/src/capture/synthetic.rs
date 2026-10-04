//! Synthetic sources for tests, benchmarks and `--synthetic` streaming: an
//! animated test pattern (a rectangle moving over a flat background, or over
//! a desktop-like picture, plus a frame counter) and a sine tone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::capture::{
	AudioCapture, BoxFuture, CaptureOptions, CaptureSource, DmaBufRef, FrameSink, QueueSink,
	ScreenCapture, SourceId, Ticker, Worker,
};
use crate::frame::{AUDIO_SAMPLE_RATE, AudioBuffer, FrameRef, PixelsRef, PlaneRef, VideoFrame};
use crate::queue::{FrameReceiver, frame_channel};
use crate::workers::{Slots, Workers};
use crate::{Error, Result};

/// Background colour of the pattern (RGB).
pub const BACKGROUND: [u8; 3] = [24, 40, 72];
/// Colour of the moving rectangle (RGB).
pub const RECT_COLOR: [u8; 3] = [240, 90, 30];
/// Colour of the frame counter (RGB).
pub const TEXT_COLOR: [u8; 3] = [255, 255, 255];

/// 3x5 bitmaps of the digits 0-9, one row per 3 bits.
const DIGITS: [[u8; 5]; 10] = [
	[7, 5, 5, 5, 7],
	[2, 6, 2, 2, 7],
	[7, 1, 7, 4, 7],
	[7, 1, 7, 1, 7],
	[5, 5, 7, 1, 1],
	[7, 4, 7, 1, 7],
	[7, 4, 7, 5, 7],
	[7, 1, 2, 2, 2],
	[7, 5, 7, 5, 7],
	[7, 5, 7, 1, 7],
];

/// What the test pattern shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Pattern {
	/// A flat background, the moving rectangle and a frame counter: cheap to
	/// encode and easy to check after decoding.
	#[default]
	Simple,
	/// Like a desktop with a code editor: a title bar with a gradient, a side
	/// bar, lines of coloured text in a document that scrolls three pixels
	/// per frame, a status bar, and the moving rectangle and frame counter on
	/// top. Costs an encoder about as much as real screen content (for
	/// benchmarks).
	Desktop,
}

/// A rectangle: `(x, y, width, height)`.
type Rect = (usize, usize, usize, usize);

fn bgra(c: [u8; 3]) -> [u8; 4] {
	[c[2], c[1], c[0], 255]
}

/// Fill a rectangle of a BGRA image (`width` pixels per row), clipped.
fn fill(data: &mut [u8], width: usize, rect: Rect, color: [u8; 3]) {
	let rows = data.len() / (width * 4).max(1);
	let (x0, y0, w, h) = rect;
	let x1 = (x0 + w).min(width);
	if x0 >= x1 {
		return;
	}
	let px = bgra(color);
	for y in y0..(y0 + h).min(rows) {
		for d in data[(y * width + x0) * 4..(y * width + x1) * 4].chunks_exact_mut(4) {
			d.copy_from_slice(&px);
		}
	}
}

/// Draw one 3x5 glyph at `scale` pixels per dot.
fn glyph(
	data: &mut [u8],
	width: usize,
	at: (usize, usize),
	scale: usize,
	bitmap: [u8; 5],
	color: [u8; 3],
) {
	for (row, bits) in bitmap.iter().enumerate() {
		for col in 0..3 {
			if bits & (4 >> col) != 0 {
				fill(data, width, (at.0 + col * scale, at.1 + row * scale, scale, scale), color);
			}
		}
	}
}

/// Draw the decimal digits of `n`.
fn number(data: &mut [u8], width: usize, at: (usize, usize), scale: usize, n: u64, color: [u8; 3]) {
	let mut digits = [0u8; 20];
	let (mut rest, mut len) = (n, 0);
	loop {
		digits[len] = (rest % 10) as u8;
		len += 1;
		rest /= 10;
		if rest == 0 {
			break;
		}
	}
	for i in 0..len {
		let bitmap = DIGITS[usize::from(digits[len - 1 - i])];
		glyph(data, width, (at.0 + i * 4 * scale, at.1), scale, bitmap, color);
	}
}

/// A small deterministic generator (xorshift64).
struct Rng(u64);

impl Rng {
	fn below(&mut self, n: usize) -> usize {
		self.0 ^= self.0 << 13;
		self.0 ^= self.0 >> 7;
		self.0 ^= self.0 << 17;
		(self.0 % n.max(1) as u64) as usize
	}
}

/// Lines of "code" in `area`: words of glyphs in the palette's colours,
/// indented, some lines empty.
fn text_lines(
	data: &mut [u8],
	width: usize,
	area: Rect,
	scale: usize,
	palette: &[[u8; 3]],
	rng: &mut Rng,
) {
	let (ax, ay, aw, ah) = area;
	let (advance, pitch) = (4 * scale, 8 * scale);
	let columns = aw.saturating_sub(2 * advance) / advance;
	let mut y = ay + 2 * scale;
	while columns > 0 && y + pitch <= ay + ah {
		let indent = rng.below(4) * 2;
		let end = if rng.below(6) == 0 { 0 } else { indent + 4 + rng.below(columns) };
		let mut col = indent;
		while col < end.min(columns) {
			let word = 2 + rng.below(8);
			let color = palette[rng.below(palette.len())];
			for _ in 0..word.min(columns - col) {
				let x = ax + advance + col * advance;
				glyph(data, width, (x, y), scale, DIGITS[rng.below(10)], color);
				col += 1;
			}
			col += 1;
		}
		y += pitch;
	}
}

/// The parts of [`Pattern::Desktop`] that do not change, rendered once: the
/// page, and the document that scrolls through the editor area.
struct Desktop {
	page: Vec<u8>,
	editor: Rect,
	/// `editor.2` pixels wide, twice the editor's height (it wraps around).
	document: Vec<u8>,
	document_rows: usize,
}

impl Desktop {
	fn render(width: usize, height: usize) -> Self {
		let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
		let scale = (height / 540).max(1);
		let mut page = vec![0; width * height * 4];
		let title = (height / 20).max(4);
		let status = (height / 40).max(2);
		let side = width / 5;
		// Title bar: a vertical gradient with tabs.
		for y in 0..title {
			let t = y * 255 / title;
			let c = [(40 + t / 8) as u8, (60 + t / 6) as u8, (110 + t / 4) as u8];
			fill(&mut page, width, (0, y, width, 1), c);
		}
		for tab in 0..6 {
			let tab = (side + tab * width / 8, title / 3, width / 9, title - title / 3);
			fill(&mut page, width, tab, [30, 30, 30]);
			text_lines(&mut page, width, tab, scale, &[[200, 200, 200]], &mut rng);
		}
		// Side bar with a file list, status bar.
		let sidebar = (0, title, side, height - title - status);
		fill(&mut page, width, sidebar, [37, 37, 38]);
		let palette = [[204, 204, 204], [150, 150, 150], [230, 200, 120]];
		text_lines(&mut page, width, sidebar, scale, &palette, &mut rng);
		fill(&mut page, width, (0, height - status, width, status), [0, 122, 204]);
		// The editor's document.
		let editor = (side, title, width - side, height - title - status);
		let document_rows = editor.3 * 2;
		let mut document = vec![0; editor.2 * document_rows * 4];
		fill(&mut document, editor.2, (0, 0, editor.2, document_rows), [30, 30, 30]);
		let palette =
			[[212, 212, 212], [86, 156, 214], [206, 145, 120], [106, 153, 85], [197, 134, 192]];
		text_lines(
			&mut document,
			editor.2,
			(0, 0, editor.2, document_rows),
			scale,
			&palette,
			&mut rng,
		);
		Self { page, editor, document, document_rows }
	}

	/// Rows `first..` of the page with the document scrolled by `offset`
	/// rows, into `out` (whole rows).
	fn draw(&self, out: &mut [u8], width: usize, first: usize, offset: usize) {
		let start = first * width * 4;
		out.copy_from_slice(&self.page[start..start + out.len()]);
		let (ex, ey, ew, eh) = self.editor;
		let rows = first..first + out.len() / (width * 4);
		for y in rows.start.max(ey)..rows.end.min(ey + eh) {
			let src = (offset + y - ey) % self.document_rows * ew * 4;
			let dst = ((y - first) * width + ex) * 4;
			out[dst..dst + ew * 4].copy_from_slice(&self.document[src..src + ew * 4]);
		}
	}
}

/// The test pattern as a [`ScreenCapture`] backend.
pub struct SyntheticScreen {
	width: u32,
	height: u32,
	pattern: Pattern,
	desktop: Option<Arc<Desktop>>,
	worker: Option<Worker>,
	dmabuf: bool,
	video_memory: bool,
}

impl SyntheticScreen {
	pub fn new(width: u32, height: u32) -> Self {
		Self::with_pattern(width, height, Pattern::Simple)
	}

	/// A test pattern of another kind.
	pub fn with_pattern(width: u32, height: u32, pattern: Pattern) -> Self {
		let (width, height) = (width.max(16), height.max(16));
		let desktop = (pattern == Pattern::Desktop)
			.then(|| Arc::new(Desktop::render(width as usize, height as usize)));
		Self { width, height, pattern, desktop, worker: None, dmabuf: false, video_memory: false }
	}

	/// Draw into a DMA-BUF of ordinary memory and offer it to sinks that
	/// take DMA-BUFs ([`FrameSink::accepts_dmabuf`]) as a LINEAR `XR24`
	/// buffer, as the ScreenCast portal does; sinks that do not, or decline
	/// it, get the same memory as a frame. Linux with the `pipewire`
	/// feature and access to `/dev/udmabuf`; elsewhere frames are handed
	/// over as before.
	pub fn with_dmabuf(mut self, enabled: bool) -> Self {
		self.dmabuf = enabled;
		self
	}

	/// With [`with_dmabuf`](Self::with_dmabuf): the DMA-BUFs are buffers in
	/// video memory in the GPU driver's own (tiled) layout, as a
	/// compositor's are, drawn once in advance ([`VIDEO_MEMORY_FRAMES`]
	/// frames of the pattern, handed over in turn) so the source costs
	/// nothing per frame. The GPU reads ordinary memory over the bus, several
	/// times slower (measured on a Radeon RX 7900 GRE: 9.7 ms against 0.16
	/// for a 3840x2160 conversion), and `/dev/udmabuf` refuses buffers of
	/// more than 64 MiB by default (7680x4320 is 132). Linux with the `ffmpeg`
	/// feature and VA-API; elsewhere, ordinary memory as before.
	pub fn with_video_memory(mut self, enabled: bool) -> Self {
		self.video_memory = enabled;
		self
	}

	pub fn pattern(&self) -> Pattern {
		self.pattern
	}

	/// Another source drawing the same pattern.
	fn same_pattern(&self) -> Self {
		Self {
			width: self.width,
			height: self.height,
			pattern: self.pattern,
			desktop: self.desktop.clone(),
			worker: None,
			dmabuf: self.dmabuf,
			video_memory: self.video_memory,
		}
	}

	/// The moving rectangle in frame `n`: `(x, y, width, height)`.
	pub fn rect(&self, n: u64) -> (u32, u32, u32, u32) {
		let (rw, rh) = ((self.width / 5).max(8), (self.height / 5).max(8));
		let range = u64::from(self.width - rw);
		let step = u64::from((self.width / 60).max(1));
		// Back and forth across the frame.
		let pos = (n * step) % (2 * range.max(1));
		let x = if pos > range { 2 * range - pos } else { pos };
		(x as u32, (self.height - rh) / 2, rw, rh)
	}

	/// Draw frame `n` as BGRA (`width * 4` bytes per row) into `out`, reusing
	/// its memory.
	pub fn render(&self, n: u64, out: &mut Vec<u8>) {
		out.resize(self.width as usize * self.height as usize * 4, 0);
		self.draw(n, out, &mut Workers::new("voelin-synthetic", 1));
	}

	/// [`render`](Self::render) into the first `width * height * 4` bytes of
	/// `out`, the background in bands of rows on `workers` (a 7680x4320
	/// frame is 130 MB to write).
	fn draw(&self, n: u64, out: &mut [u8], workers: &mut Workers) {
		let (w, h) = (self.width as usize, self.height as usize);
		let out = &mut out[..w * h * 4];
		let tasks = workers.tasks(h);
		let band = h.div_ceil(tasks);
		let bands = Slots::new(out.chunks_mut(band * w * 4).enumerate());
		let background = bgra(BACKGROUND);
		workers.run(bands.len(), &|i| {
			let Some((i, rows)) = bands.take(i) else { return };
			match &self.desktop {
				Some(desktop) => desktop.draw(rows, w, i * band, n as usize * 3),
				None => {
					for px in rows.chunks_exact_mut(4) {
						px.copy_from_slice(&background);
					}
				}
			}
		});
		let (rx, ry, rw, rh) = self.rect(n);
		fill(out, w, (rx as usize, ry as usize, rw as usize, rh as usize), RECT_COLOR);
		// Frame counter in the top-left corner.
		let scale = (h / 48).max(1);
		number(out, w, (scale * 2, scale * 2), scale, n, TEXT_COLOR);
	}

	/// Frame `n` as BGRA, stamped `n / fps` seconds.
	pub fn frame(&self, n: u64, fps: u32) -> VideoFrame {
		let mut data = Vec::new();
		self.render(n, &mut data);
		let timestamp = Duration::from_secs(n) / fps.max(1);
		VideoFrame::from_bgra(self.width, self.height, self.width as usize * 4, data)
			.expect("pattern buffer matches its size")
			.with_timestamp(timestamp)
	}
}

impl ScreenCapture for SyntheticScreen {
	fn backend(&self) -> &'static str {
		"synthetic"
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		Ok(vec![CaptureSource {
			id: SourceId::Synthetic,
			name: "Test pattern".into(),
			width: self.width,
			height: self.height,
			primary: true,
		}])
	}

	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<FrameReceiver<VideoFrame>>> {
		let (sink, rx) = QueueSink::new(options);
		let started = self.start_sink(source, options, Box::new(sink));
		Box::pin(async move {
			started.await?;
			Ok(rx)
		})
	}

	/// Draws frame after frame into one buffer, at [`FrameSink::max_fps`];
	/// timestamps are the time since the start.
	fn start_sink(
		&mut self,
		source: &SourceId,
		_options: &CaptureOptions,
		mut sink: Box<dyn FrameSink>,
	) -> BoxFuture<'_, Result<()>> {
		let source = source.clone();
		Box::pin(async move {
			if source != SourceId::Synthetic {
				return Err(Error::SourceNotFound(source));
			}
			self.stop();
			let pattern = self.same_pattern();
			self.worker = Some(Worker::spawn("voelin-synthetic-video", move |stop| {
				let (width, height) = (pattern.width, pattern.height);
				let mut fps = sink.max_fps();
				let mut ticker = Ticker::new(fps);
				let mut buffer = Buffer::new(&pattern);
				// A few threads: drawing is memory-bound, and many threads
				// cost far more CPU for little time.
				let mut workers = Workers::new("voelin-synthetic", 4);
				let started = Instant::now();
				let mut n = 0;
				loop {
					let timestamp = started.elapsed();
					if sink.wants(timestamp) {
						// Buffers in video memory are drawn in advance; memory
						// only for a sink that declines them.
						if !buffer.drawn_in_advance() {
							pattern.draw(n, buffer.bytes(), &mut workers);
						}
						let taken = match buffer.dmabuf(&pattern, n, timestamp) {
							Some(frame) if sink.accepts_dmabuf() => sink.dmabuf(&frame),
							_ => None,
						};
						let more = taken.unwrap_or_else(|| {
							if buffer.drawn_in_advance() {
								pattern.draw(n, buffer.bytes(), &mut workers);
							}
							let plane = PlaneRef::new(buffer.bytes(), width as usize * 4);
							sink.frame(FrameRef {
								width,
								height,
								timestamp,
								pixels: PixelsRef::Bgra(plane),
							})
						});
						if !more {
							break;
						}
						n += 1;
					}
					if sink.max_fps() != fps {
						fps = sink.max_fps();
						ticker.set_fps(fps);
					}
					if !ticker.wait(&stop) {
						break;
					}
				}
			})?);
			Ok(())
		})
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

/// Frames of the pattern drawn in advance into video memory
/// ([`SyntheticScreen::with_video_memory`]), handed over in turn.
pub const VIDEO_MEMORY_FRAMES: usize = 8;

/// Where the test pattern is drawn: plain memory, a DMA-BUF of ordinary
/// memory ([`SyntheticScreen::with_dmabuf`]), or buffers in video memory
/// drawn in advance (and memory for a sink that declines them).
enum Buffer {
	Memory(Vec<u8>),
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	Dmabuf(super::dmabuf::Udmabuf),
	#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
	Video(Vec<crate::ffmpeg::Exported>, Vec<u8>),
}

impl Buffer {
	fn new(pattern: &SyntheticScreen) -> Self {
		let len = pattern.width as usize * pattern.height as usize * 4;
		#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
		if pattern.dmabuf && pattern.video_memory {
			match Self::video_memory(pattern) {
				Ok(frames) => return Self::Video(frames, vec![0; len]),
				Err(e) => tracing::warn!("test pattern not in video memory: {e}"),
			}
		}
		#[cfg(all(target_os = "linux", feature = "pipewire"))]
		if pattern.dmabuf {
			match super::dmabuf::Udmabuf::new(len) {
				Ok(buffer) => return Self::Dmabuf(buffer),
				Err(e) => tracing::warn!("test pattern without DMA-BUFs (/dev/udmabuf): {e}"),
			}
		}
		Self::Memory(vec![0; len])
	}

	/// [`VIDEO_MEMORY_FRAMES`] frames of the pattern in VA-API surfaces of
	/// the driver's layout, exported as DMA-BUFs.
	#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
	fn video_memory(
		pattern: &SyntheticScreen,
	) -> std::result::Result<Vec<crate::ffmpeg::Exported>, String> {
		let ffmpeg = crate::ffmpeg::Ffmpeg::get().map_err(str::to_owned)?;
		let bgr0 = ffmpeg.pix.bgr0.ok_or("no bgr0 pixel format")?;
		let size = (pattern.width, pattern.height);
		(0..VIDEO_MEMORY_FRAMES as u64)
			.map(|n| {
				let picture = pattern.frame(n, 1);
				crate::ffmpeg::Exported::new(size, bgr0, super::drm_fourcc(b"XR24"), Some(&picture))
			})
			.collect()
	}

	/// The frames are in video memory already.
	fn drawn_in_advance(&self) -> bool {
		#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
		if let Self::Video(..) = self {
			return true;
		}
		false
	}

	fn bytes(&mut self) -> &mut [u8] {
		match self {
			Self::Memory(bytes) => bytes,
			#[cfg(all(target_os = "linux", feature = "pipewire"))]
			Self::Dmabuf(buffer) => buffer.bytes_mut(),
			#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
			Self::Video(_, bytes) => bytes,
		}
	}

	/// Frame `n` as an `XR24` DMA-BUF, if the buffers are DMA-BUFs.
	fn dmabuf(&self, pattern: &SyntheticScreen, n: u64, timestamp: Duration) -> Option<DmaBufRef> {
		match self {
			Self::Memory(_) => {
				let _ = (pattern, n, timestamp);
				None
			}
			#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
			Self::Video(frames, _) => {
				let exported = &frames[n as usize % frames.len()];
				Some(DmaBufRef { timestamp, ..exported.frame })
			}
			#[cfg(all(target_os = "linux", feature = "pipewire"))]
			Self::Dmabuf(buffer) => Some(DmaBufRef {
				width: pattern.width,
				height: pattern.height,
				timestamp,
				fourcc: super::drm_fourcc(b"XR24"),
				modifier: super::DRM_MOD_LINEAR,
				fd: buffer.fd(),
				size: buffer.len(),
				planes: [(0, pattern.width as usize * 4), (0, 0), (0, 0), (0, 0)],
				plane_count: 1,
			}),
		}
	}
}

/// A sine tone as an [`AudioCapture`] backend: 10 ms stereo buffers.
pub struct SineSource {
	frequency: f32,
	amplitude: f32,
	worker: Option<Worker>,
}

impl SineSource {
	/// Samples per channel in one buffer (10 ms).
	pub const BUFFER_FRAMES: usize = AUDIO_SAMPLE_RATE as usize / 100;

	pub fn new(frequency: f32, amplitude: f32) -> Self {
		Self { frequency, amplitude, worker: None }
	}

	/// Buffer `n` of the tone (continuous across buffers).
	pub fn buffer(&self, n: u64) -> AudioBuffer {
		let start = n * Self::BUFFER_FRAMES as u64;
		let mut samples = Vec::with_capacity(Self::BUFFER_FRAMES * 2);
		for i in 0..Self::BUFFER_FRAMES as u64 {
			let t = (start + i) as f64 / f64::from(AUDIO_SAMPLE_RATE);
			let s = f64::from(self.amplitude)
				* (2.0 * std::f64::consts::PI * f64::from(self.frequency) * t).sin();
			samples.extend([s as f32; 2]);
		}
		AudioBuffer { samples, channels: 2, timestamp: Duration::from_millis(n * 10) }
	}
}

impl AudioCapture for SineSource {
	fn backend(&self) -> &'static str {
		"synthetic"
	}

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>> {
		self.stop();
		// Half a second of audio before the oldest is dropped.
		let (tx, rx) = frame_channel(50);
		let tone = SineSource::new(self.frequency, self.amplitude);
		self.worker = Some(Worker::spawn("voelin-synthetic-audio", move |stop| {
			let mut ticker = Ticker::new(100);
			let mut n = 0;
			while tx.send(tone.buffer(n)) && ticker.wait(&stop) {
				n += 1;
			}
		})?);
		Ok(rx)
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::convert;

	fn rgb_at(frame: &VideoFrame, x: u32, y: u32) -> [u8; 3] {
		let rgba = convert::to_rgba_vec(frame).unwrap();
		let i = ((y * frame.width + x) * 4) as usize;
		[rgba[i], rgba[i + 1], rgba[i + 2]]
	}

	#[test]
	fn pattern_moves_and_counts() {
		let screen = SyntheticScreen::new(320, 240);
		let (x0, y0, w, h) = screen.rect(0);
		let (x1, ..) = screen.rect(10);
		assert_ne!(x0, x1);
		let frame = screen.frame(10, 30);
		assert_eq!(frame.timestamp, Duration::from_secs(10) / 30);
		assert_eq!(rgb_at(&frame, x1 + w / 2, y0 + h / 2), RECT_COLOR);
		assert_eq!(rgb_at(&frame, 319, 239), BACKGROUND);
		// The "1" of "10" lights its middle column at the top.
		let scale = 240 / 48;
		assert_eq!(rgb_at(&frame, (scale * 2 + scale) as u32, (scale * 2) as u32), TEXT_COLOR);
		// The rectangle bounces within the frame.
		for n in 0..500 {
			let (x, _, w, _) = screen.rect(n);
			assert!(x + w <= 320);
		}
	}

	#[test]
	fn desktop_pattern_scrolls() {
		let screen = SyntheticScreen::with_pattern(640, 360, Pattern::Desktop);
		assert_eq!(screen.pattern(), Pattern::Desktop);
		let (mut a, mut b) = (Vec::new(), Vec::new());
		screen.render(1, &mut a);
		screen.render(2, &mut b);
		assert_eq!(a.len(), 640 * 360 * 4);
		// The editor (right of the side bar, below the title bar) scrolled.
		let row = |data: &[u8], y: usize| data[(y * 640 + 200) * 4..(y * 640 + 600) * 4].to_vec();
		let differs = (40..340).any(|y| row(&a, y) != row(&b, y));
		assert!(differs);
		// Lots of detail: many distinct colours in the editor.
		let mut colours: Vec<&[u8]> = a.chunks_exact(4).collect();
		colours.sort_unstable();
		colours.dedup();
		assert!(colours.len() >= 8, "{} colours", colours.len());
		// Rendering reuses the buffer.
		let capacity = a.capacity();
		screen.render(3, &mut a);
		assert_eq!(a.capacity(), capacity);
	}

	#[test]
	fn sine_is_continuous() {
		let tone = SineSource::new(1000.0, 0.5);
		let a = tone.buffer(0);
		let b = tone.buffer(1);
		assert_eq!(a.frames(), 480);
		let last = a.samples[a.samples.len() - 2];
		let next = b.samples[0];
		// Adjacent samples of a 1 kHz tone differ by at most 2*pi*f/rate*amp.
		assert!((last - next).abs() < 0.07, "{last} -> {next}");
		assert!(a.samples.iter().all(|s| s.abs() <= 0.5));
	}

	#[tokio::test]
	async fn capture_runs_until_stopped() {
		let mut screen = SyntheticScreen::new(64, 48);
		let options = CaptureOptions { fps: 50, ..CaptureOptions::default() };
		assert!(screen.start(&SourceId::Monitor(0), &options).await.is_err());
		let mut frames = screen.start(&SourceId::Synthetic, &options).await.unwrap();
		let first = frames.recv().await.unwrap();
		let second = frames.recv().await.unwrap();
		assert_eq!((first.width, first.height), (64, 48));
		assert!(second.timestamp > first.timestamp);
		screen.stop();
		// The worker is gone: the channel closes after the queued frames.
		while frames.recv().await.is_some() {}

		let mut tone = SineSource::new(440.0, 0.25);
		let mut audio = tone.start().unwrap();
		let buffer = audio.recv_timeout(Duration::from_secs(2)).unwrap();
		assert_eq!((buffer.channels, buffer.frames()), (2, 480));
		tone.stop();
	}

	/// With DMA-BUFs, a sink that takes them gets the pattern as a LINEAR
	/// `XR24` DMA-BUF that reads back as the pattern; one that declines it
	/// gets the same picture as a frame.
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	#[tokio::test]
	async fn the_pattern_as_dmabufs() {
		use std::sync::mpsc;

		/// Takes DMA-BUFs (or declines them) and reports what it got.
		struct Sink {
			take: bool,
			got: mpsc::Sender<(bool, Vec<u8>)>,
		}
		impl FrameSink for Sink {
			fn max_fps(&self) -> u32 {
				50
			}
			fn frame(&mut self, frame: FrameRef<'_>) -> bool {
				let PixelsRef::Bgra(plane) = frame.pixels else { panic!("BGRA") };
				self.got.send((false, plane.row(0, 64 * 4).to_vec())).is_ok()
			}
			fn accepts_dmabuf(&self) -> bool {
				true
			}
			fn dmabuf(&mut self, frame: &DmaBufRef) -> Option<bool> {
				assert_eq!(frame.fourcc, crate::capture::drm_fourcc(b"XR24"));
				assert_eq!((frame.width, frame.height, frame.planes[0].1), (64, 48, 256));
				if !self.take {
					return None;
				}
				let map = super::super::dmabuf::DmaBufMap::new(frame.fd, frame.size).unwrap();
				let row = map.read(|bytes| bytes[..64 * 4].to_vec());
				Some(self.got.send((true, row)).is_ok())
			}
		}

		if !std::path::Path::new("/dev/udmabuf").exists() {
			eprintln!("no /dev/udmabuf, skipped");
			return;
		}
		let mut expected = Vec::new();
		SyntheticScreen::new(64, 48).render(0, &mut expected);
		for take in [true, false] {
			let mut screen = SyntheticScreen::new(64, 48).with_dmabuf(true);
			let (got, frames) = mpsc::channel();
			let sink = Box::new(Sink { take, got });
			screen
				.start_sink(&SourceId::Synthetic, &CaptureOptions::default(), sink)
				.await
				.unwrap();
			let (dmabuf, row) = frames.recv_timeout(Duration::from_secs(5)).unwrap();
			screen.stop();
			assert_eq!(dmabuf, take, "taken as a DMA-BUF");
			assert_eq!(row, expected[..64 * 4], "the first row of frame 0");
		}
	}

	/// In video memory: the frames drawn in advance come in turn, each its
	/// own buffer; a sink that declines them gets memory frames.
	#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
	#[tokio::test]
	async fn the_pattern_in_video_memory() {
		use std::sync::mpsc;

		/// Takes DMA-BUFs (or declines them) and reports each one's fd.
		struct Sink {
			take: bool,
			got: mpsc::Sender<Option<i32>>,
		}
		impl FrameSink for Sink {
			fn max_fps(&self) -> u32 {
				100
			}
			fn frame(&mut self, _: FrameRef<'_>) -> bool {
				self.got.send(None).is_ok()
			}
			fn accepts_dmabuf(&self) -> bool {
				true
			}
			fn dmabuf(&mut self, frame: &DmaBufRef) -> Option<bool> {
				assert_eq!((frame.width, frame.height), (256, 128));
				self.take.then(|| self.got.send(Some(frame.fd)).is_ok())
			}
		}

		let exports =
			crate::ffmpeg::Ffmpeg::get().ok().and_then(|f| f.pix.bgr0).is_some_and(|bgr0| {
				crate::ffmpeg::Exported::new(
					(64, 64),
					bgr0,
					super::super::drm_fourcc(b"XR24"),
					None,
				)
				.is_ok()
			});
		if !exports {
			eprintln!("no VA-API surfaces to export, skipped");
			return;
		}
		for take in [true, false] {
			let mut screen =
				SyntheticScreen::new(256, 128).with_dmabuf(true).with_video_memory(true);
			let (got, frames) = mpsc::channel();
			let sink = Box::new(Sink { take, got });
			screen
				.start_sink(&SourceId::Synthetic, &CaptureOptions::default(), sink)
				.await
				.unwrap();
			let mut fds = std::collections::BTreeSet::new();
			for _ in 0..2 * VIDEO_MEMORY_FRAMES {
				match frames.recv_timeout(Duration::from_secs(5)).unwrap() {
					Some(fd) => {
						assert!(take, "declined, yet a DMA-BUF");
						fds.insert(fd);
					}
					None => assert!(!take, "taken, yet a memory frame"),
				}
			}
			screen.stop();
			if take {
				assert_eq!(fds.len(), VIDEO_MEMORY_FRAMES, "each buffer in turn");
			}
		}
	}
}
