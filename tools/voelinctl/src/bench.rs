//! `voelinctl stream bench`: the streamer's capture → convert → encode
//! pipeline on the test pattern, without a server. Prints frame rates, bit
//! rates and heap allocations per frame in the steady state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use voelin_core::media::voelin_media::capture::SourceId;
use voelin_core::media::voelin_media::capture::synthetic::Pattern;
use voelin_core::media::voelin_media::{Codec, Codecs};
use voelin_core::media::{EncoderPreference, MediaSink, Streamer, StreamerConfig, preferred_codec};
use voelin_stream::{EncodedFrame, LayerId, LayerSet, LayerSpec, MediaKind};

use crate::alloc;

#[derive(Args, Debug)]
pub struct StreamToolArgs {
	#[command(subcommand)]
	command: StreamTool,
}

#[derive(Subcommand, Debug)]
enum StreamTool {
	/// Run the streamer's pipeline (capture → convert/scale → encode) on the
	/// test pattern without a server; print per-stage and per-layer numbers
	/// and heap allocations per frame.
	Bench(BenchArgs),
	/// List the video encoders: the FFmpeg libraries found, every backend
	/// with its self-test result (or why it cannot be used), and the order
	/// the streamer would use them in.
	Encoders(EncodersArgs),
}

#[derive(Args, Debug)]
struct EncodersArgs {
	/// Encoder choice to rank by: `auto`, `software` or a backend name (as
	/// the setting `stream.encoder_backend`).
	#[arg(long, default_value = "auto")]
	encoder: String,
	/// Rank without hardware encoders (`stream.hardware_acceleration` off).
	#[arg(long)]
	no_hardware: bool,
	/// Cisco's OpenH264 library, to list it as well.
	#[arg(long)]
	openh264: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct BenchArgs {
	/// What to capture: `synthetic` (the test pattern), `portal` (the
	/// desktop's screen-share dialog, Wayland and X11), `monitor:<n>` or
	/// `window:<id>` (X11). Anything but `synthetic` ignores --res and
	/// --pattern and measures the real capture → convert → encode pipeline.
	#[arg(long, default_value = "synthetic")]
	source: String,
	/// Give up if the capture has not started after this many seconds (the
	/// portal opens a dialog the user has to accept).
	#[arg(long, default_value_t = 60)]
	start_timeout: u64,
	/// Size of the test pattern.
	#[arg(long, default_value = "1920x1080")]
	res: String,
	/// Frame rate of the capture.
	#[arg(long, default_value_t = 30)]
	fps: u32,
	/// Test pattern: `desktop` (text, a scrolling document; about as costly
	/// as real screen content) or `simple` (a flat background).
	#[arg(long, default_value = "desktop")]
	pattern: String,
	/// Video codec: vp8, vp9, h264, av1 or h265 [default: the codec of
	/// --encoder if named, else the first the encoder choice gives].
	#[arg(long)]
	codec: Option<String>,
	/// Encoder: `auto`, `software` or a backend name from `voelinctl stream
	/// encoders` (`h264_vaapi`, `libx264`, `libsvtav1`, ...).
	#[arg(long, default_value = "auto")]
	encoder: String,
	/// No hardware encoders unless named with --encoder.
	#[arg(long)]
	no_hardware: bool,
	/// A simulcast layer, e.g. `scale=0.5,bitrate=1500k,fps=15` (keys: id,
	/// scale, size=WxH, fps, bitrate, max, min, rid; bitrates in bit/s with
	/// k/M suffixes); repeatable. Without it: one layer at --bitrate.
	#[arg(long = "layer")]
	layers: Vec<String>,
	/// Bitrate in kbit/s without --layer.
	#[arg(long, default_value_t = 4608)]
	bitrate: u32,
	/// Measured time in seconds (after the warm-up).
	#[arg(long, default_value_t = 10)]
	seconds: u64,
	/// Seconds to run before measuring.
	#[arg(long, default_value_t = 2)]
	warmup: u64,
	/// Cisco's OpenH264 library, for --codec h264.
	#[arg(long)]
	openh264: Option<PathBuf>,
}

pub fn run(args: StreamToolArgs) -> Result<()> {
	match args.command {
		StreamTool::Bench(args) => bench(args),
		StreamTool::Encoders(args) => encoders(args),
	}
}

/// `Codecs` with the encoder choice of the arguments.
fn codecs_for(encoder: &str, no_hardware: bool, openh264: Option<&PathBuf>) -> Result<Codecs> {
	let preference =
		EncoderPreference { hardware: !no_hardware, backend: encoder.parse().unwrap_or_default() };
	let mut codecs = Codecs::new().with_preference(preference);
	if let Some(path) = openh264 {
		let library = voelin_core::media::voelin_media::codec::h264::OpenH264::load(path)
			.context("OpenH264")?;
		codecs = codecs.with_openh264(library);
	}
	Ok(codecs)
}

fn encoders(args: EncodersArgs) -> Result<()> {
	let codecs = codecs_for(&args.encoder, args.no_hardware, args.openh264.as_ref())?;
	let report = codecs.report();
	match &report.ffmpeg {
		Ok(text) => println!("{text}"),
		Err(e) => println!("FFmpeg: not used ({e})"),
	}
	match &report.zero_copy {
		Ok(()) => println!("zero-copy DMA-BUF import: available"),
		Err(e) => println!("zero-copy DMA-BUF import: no ({e})"),
	}
	println!();
	println!("{:<18} {:<5} {:<17} {:<8} {:<5} status", "encoder", "codec", "api", "kind", "rank");
	let mut list = report.encoders.clone();
	// Usable ones first, in the order the streamer tries them per codec.
	list.sort_by_key(|e| (e.status.is_err(), e.rank.unwrap_or(usize::MAX), e.codec.name()));
	for e in &list {
		let status = match &e.status {
			Ok(()) if e.rank.is_none() => "ok (only when named)".to_owned(),
			Ok(()) => "ok".to_owned(),
			Err(reason) => reason.clone(),
		};
		println!(
			"{:<18} {:<5} {:<17} {:<8} {:<5} {status}",
			e.name,
			e.codec.name(),
			e.api,
			if e.hardware { "hardware" } else { "software" },
			e.rank.map_or("-".to_owned(), |r| r.to_string()),
		);
	}
	let order: Vec<String> =
		codecs.encoders().iter().map(|(codec, backend)| format!("{codec} {backend}")).collect();
	println!("\nstreamer order: {}", order.join(", "));
	Ok(())
}

/// Frames and bytes the sink got for one layer.
#[derive(Default)]
struct LayerCount {
	frames: AtomicU64,
	keyframes: AtomicU64,
	bytes: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LayerSnapshot {
	frames: u64,
	keyframes: u64,
	bytes: u64,
}

/// Counts what the streamer sends; asks for one keyframe per layer at the
/// start, like a new viewer.
struct BenchSink {
	layers: Vec<(LayerId, LayerCount)>,
	keyframe: AtomicBool,
}

impl BenchSink {
	fn snapshot(&self) -> Vec<(LayerId, LayerSnapshot)> {
		self.layers
			.iter()
			.map(|(id, c)| {
				let snapshot = LayerSnapshot {
					frames: c.frames.load(Ordering::Relaxed),
					keyframes: c.keyframes.load(Ordering::Relaxed),
					bytes: c.bytes.load(Ordering::Relaxed),
				};
				(*id, snapshot)
			})
			.collect()
	}
}

impl MediaSink for BenchSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		if frame.kind != MediaKind::Video {
			return true;
		}
		if let Some((_, count)) = self.layers.iter().find(|(id, _)| *id == frame.layer) {
			count.frames.fetch_add(1, Ordering::Relaxed);
			count.keyframes.fetch_add(u64::from(frame.keyframe), Ordering::Relaxed);
			count.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
		}
		true
	}

	fn take_keyframe_request(&self) -> bool {
		self.keyframe.swap(false, Ordering::Relaxed)
	}

	fn take_layer_keyframes(&self, layers: &mut LayerSet) {
		if self.keyframe.swap(false, Ordering::Relaxed) {
			for (id, _) in &self.layers {
				layers.insert(*id);
			}
		}
	}
}

/// `synthetic`, `portal`, `monitor:<n>` or `window:<id>`.
fn parse_source(value: &str) -> Result<SourceId> {
	Ok(match value.split_once(':') {
		Some(("monitor", n)) => SourceId::Monitor(n.parse().context("monitor index")?),
		Some(("window", id)) => SourceId::Window(id.parse().context("window id")?),
		_ => match value {
			"synthetic" => SourceId::Synthetic,
			"portal" => SourceId::Portal,
			other => bail!("unknown source {other:?}: synthetic, portal, monitor:<n>, window:<id>"),
		},
	})
}

/// `<width>x<height>`.
fn parse_size(size: &str) -> Result<(u32, u32)> {
	let parsed = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
	parsed.with_context(|| format!("bad size {size:?}, expected e.g. 1280x720"))
}

/// Bit/s with an optional `k` / `M` suffix.
fn parse_bitrate(value: &str) -> Result<u64> {
	let (number, factor) = match value.as_bytes().last() {
		Some(b'k' | b'K') => (&value[..value.len() - 1], 1_000.0),
		Some(b'm' | b'M') => (&value[..value.len() - 1], 1_000_000.0),
		_ => (value, 1.0),
	};
	let number: f64 = number.parse().with_context(|| format!("bad bitrate {value:?}"))?;
	Ok((number * factor) as u64)
}

/// `key=value,...` (see `--layer`).
fn parse_layer(spec: &str, index: usize) -> Result<LayerSpec> {
	let mut layer = LayerSpec::single(1_000_000);
	layer.id = index as LayerId;
	for part in spec.split(',').filter(|p| !p.is_empty()) {
		let (key, value) = part.split_once('=').with_context(|| format!("{part:?}: no `=`"))?;
		match key {
			"id" => layer.id = value.parse().context("layer id")?,
			"scale" => layer.scale = value.parse().context("layer scale")?,
			"size" => layer.size = Some(parse_size(value)?),
			"fps" => layer.max_fps = Some(value.parse().context("layer fps")?),
			"bitrate" => layer.bitrate = parse_bitrate(value)?,
			"max" => layer.max_bitrate = Some(parse_bitrate(value)?),
			"min" => layer.min_bitrate = parse_bitrate(value)?,
			"rid" => layer.rid = Some(value.to_owned()),
			_ => bail!("unknown layer key {key:?} in {spec:?}"),
		}
	}
	Ok(layer)
}

/// CPU time of this process (user + system), Linux only.
fn cpu_time() -> Option<Duration> {
	let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
	// Fields after the command name, which is in parentheses.
	let rest = stat.rsplit_once(')')?.1;
	let fields: Vec<&str> = rest.split_whitespace().collect();
	// utime and stime are fields 14 and 15 (1-based), in clock ticks of
	// 1/100 s on Linux.
	let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
	Some(Duration::from_millis(ticks * 10))
}

struct Sample {
	at: Instant,
	allocations: u64,
	bytes: u64,
	cpu: Option<Duration>,
	layers: Vec<(LayerId, LayerSnapshot)>,
	sent: u64,
	captured: u64,
	/// Frames the handoffs to the encoders dropped, per layer.
	dropped: Vec<(LayerId, u64)>,
}

fn sample(sink: &BenchSink, streamer: &Streamer) -> Sample {
	// Stats first: reading them allocates, which must not count.
	let stats = streamer.stats();
	let (allocations, bytes) = alloc::counts();
	Sample {
		at: Instant::now(),
		allocations,
		bytes,
		cpu: cpu_time(),
		layers: sink.snapshot(),
		sent: stats.video_frames,
		captured: stats.captured_frames,
		dropped: stats.layers.iter().map(|l| (l.id, l.dropped)).collect(),
	}
}

fn bench(args: BenchArgs) -> Result<()> {
	let (width, height) = parse_size(&args.res)?;
	let pattern = match args.pattern.as_str() {
		"desktop" => Pattern::Desktop,
		"simple" => Pattern::Simple,
		other => bail!("unknown pattern {other:?}: desktop or simple"),
	};
	let codecs = codecs_for(&args.encoder, args.no_hardware, args.openh264.as_ref())?;
	let codec: Codec = match &args.codec {
		Some(codec) => codec.parse().map_err(|e| anyhow::anyhow!("{e}"))?,
		None => {
			// The named encoder's codec, else the first of the choice.
			let named = codecs.encoders().into_iter().find(|(_, b)| b.name() == args.encoder);
			match named {
				Some((codec, _)) => codec,
				None => preferred_codec(&codecs, None).context("no video encoder")?,
			}
		}
	};
	let layers = args
		.layers
		.iter()
		.enumerate()
		.map(|(i, spec)| parse_layer(spec, i))
		.collect::<Result<Vec<_>>>()?;
	let ids: Vec<LayerId> =
		if layers.is_empty() { vec![0] } else { layers.iter().map(|l| l.id).collect() };
	let source = parse_source(&args.source)?;
	let config = StreamerConfig {
		source: source.clone(),
		synthetic_size: (width, height),
		synthetic_pattern: pattern,
		fps: args.fps,
		bitrate_kbps: args.bitrate,
		codec,
		encoder: codecs.preference().clone(),
		audio: false,
		layers: layers.clone(),
		..StreamerConfig::default()
	};
	let runtime = tokio::runtime::Runtime::new()?;
	let timeout = Duration::from_secs(args.start_timeout.max(1));
	// The portal opens a dialog on the desktop; without an answer the start
	// never returns, so it is given a deadline instead of hanging.
	let started = runtime
		.block_on(async { tokio::time::timeout(timeout, Streamer::start(&codecs, config)).await });
	let streamer = match started {
		Ok(result) => result.context("streamer")?,
		Err(_) => bail!(
			"the capture did not start within {} s (the {} source was not accepted)",
			timeout.as_secs(),
			args.source
		),
	};
	let sink = Arc::new(BenchSink {
		layers: ids.iter().map(|id| (*id, LayerCount::default())).collect(),
		keyframe: AtomicBool::new(true),
	});
	streamer.attach(sink.clone());
	let backend = streamer.stats().layers.first().and_then(|l| l.backend);
	let what = if source == SourceId::Synthetic {
		format!("{width}x{height} {} test pattern", args.pattern)
	} else {
		format!("{} capture", args.source)
	};
	println!(
		"{what} at {} fps, {codec} ({}), {} layer(s), {} s after {} s warm-up",
		args.fps,
		backend.map_or("?".to_owned(), |b| b.to_string()),
		ids.len(),
		args.seconds,
		args.warmup
	);
	// A real source only delivers once the user has picked something.
	if source != SourceId::Synthetic {
		let deadline = Instant::now() + timeout;
		while streamer.stats().captured_frames == 0 {
			if Instant::now() >= deadline {
				bail!(
					"no frame from the {} source within {} s (nobody accepted the dialog?)",
					args.source,
					timeout.as_secs()
				);
			}
			std::thread::sleep(Duration::from_millis(100));
		}
		let stats = streamer.stats();
		println!("capturing {}x{}", stats.width, stats.height);
	}
	std::thread::sleep(Duration::from_secs(args.warmup));
	let start = sample(&sink, &streamer);
	std::thread::sleep(Duration::from_secs(args.seconds));
	let end = sample(&sink, &streamer);
	let stats = streamer.stats();
	drop(streamer);

	let secs = (end.at - start.at).as_secs_f64();
	let captured = end.captured - start.captured;
	let ms = |d: Duration| d.as_secs_f64() * 1000.0;
	println!(
		"capture: {:.1} fps; convert + scale: {:.2} ms per frame on {} threads",
		captured as f64 / secs,
		ms(stats.convert_time),
		stats.convert_threads,
	);
	let mut output_frames = 0;
	for ((id, a), (_, b)) in start.layers.iter().zip(&end.layers) {
		let frames = b.frames - a.frames;
		output_frames += frames;
		let spec = layers.iter().find(|l| l.id == *id);
		let size = spec.map_or((width, height), |l| l.output_size(width, height));
		let dropped = |s: &Sample| s.dropped.iter().find(|d| d.0 == *id).map_or(0, |d| d.1);
		let layer = stats.layers.iter().find(|l| l.id == *id);
		println!(
			"layer {id}: {}x{}  {:.1} fps  {:.0} kbit/s  {} keyframes  {} dropped  encode {:.2} ms \
			 ({} threads, speed {})",
			size.0,
			size.1,
			frames as f64 / secs,
			(b.bytes - a.bytes) as f64 * 8.0 / secs / 1000.0,
			b.keyframes - a.keyframes,
			dropped(&end) - dropped(&start),
			layer.map_or(0.0, |l| ms(l.encode_time)),
			layer.map_or(0, |l| l.threads),
			layer.and_then(|l| l.speed).map_or("-".into(), |s| s.to_string()),
		);
	}
	if let Some(e) = &stats.error {
		println!("encoder error: {e}");
	}
	if let (Some(a), Some(b)) = (start.cpu, end.cpu) {
		println!("cpu: {:.2} cores", (b - a).as_secs_f64() / secs);
	}
	let allocations = end.allocations - start.allocations;
	let bytes = end.bytes - start.bytes;
	let sent = (end.sent - start.sent).max(1);
	println!(
		"heap: {allocations} allocations: {:.2} per captured frame, {:.2} per encoded frame \
		 ({:.0} bytes per encoded frame)",
		allocations as f64 / captured.max(1) as f64,
		allocations as f64 / sent as f64,
		bytes as f64 / sent as f64,
	);
	if output_frames == 0 {
		bail!("no frames were encoded");
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn layer_specs() {
		let layer = parse_layer("scale=0.5,bitrate=1.5M,fps=15,rid=m", 2).unwrap();
		assert_eq!(layer.id, 2);
		assert_eq!(layer.scale, 0.5);
		assert_eq!(layer.bitrate, 1_500_000);
		assert_eq!(layer.max_fps, Some(15));
		assert_eq!(layer.rid.as_deref(), Some("m"));
		let layer = parse_layer("id=7,size=640x360,bitrate=500k,max=800k,min=300k", 0).unwrap();
		assert_eq!((layer.id, layer.size), (7, Some((640, 360))));
		assert_eq!((layer.max_bitrate, layer.min_bitrate), (Some(800_000), 300_000));
		assert!(parse_layer("speed=3", 0).is_err());
		assert!(parse_layer("scale", 0).is_err());
	}
}
