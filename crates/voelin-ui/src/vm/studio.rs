//! The Stream Studio's view models: scene, source, mixer and layer rows,
//! the header's numbers, the source dialog's form, where new sources go and
//! the overlay sources the stream settings' switches keep in the scenes.

use std::time::Duration;

use voelin_core::settings::{AudioSourceKindSetting, AudioSourceSetting, LayerSetting};
use voelin_core::studio::SourceChange;
use voelin_core::studio::SourceStats;
use voelin_core::studio::scene::{
	Align, Background, Colour, Crop, Fit, Scene, Scenes, Source, SourceKind, Transform,
};

use crate::app::{StudioAudio, StudioLayer, StudioScene, StudioSource, StudioSourceForm};

/// The scenes; the live one marked, each with what it shows.
pub fn scenes(scenes: &Scenes) -> Vec<StudioScene> {
	let live = scenes.active().map(|s| s.id);
	scenes
		.scenes
		.iter()
		.map(|scene| StudioScene {
			id: scene.id as i32,
			name: scene.name.clone().into(),
			detail: summary(scene).into(),
			live: Some(scene.id) == live,
			kind: scene.sources.first().map_or("", |s| s.kind.label()).into(),
		})
		.collect()
}

/// "Screen + Camera": the names of the visible sources, back first (the
/// main picture, then what is over it).
fn summary(scene: &Scene) -> String {
	let names: Vec<&str> = scene.sources.iter().filter(|s| s.visible).map(Source::label).collect();
	if names.is_empty() { "Empty".into() } else { names.join(" + ") }
}

pub fn background_name(background: &Background) -> &'static str {
	match background {
		Background::Keep => "keep",
		Background::Blur { .. } => "blur",
		Background::Image { .. } => "image",
		Background::Colour { .. } => "colour",
	}
}

/// The sources of `scene`, front first, with what the studio reports.
pub fn sources(scene: Option<&Scene>, stats: &[SourceStats]) -> Vec<StudioSource> {
	let Some(scene) = scene else { return Vec::new() };
	let last = scene.sources.len().saturating_sub(1);
	scene
		.sources
		.iter()
		.enumerate()
		.rev()
		.map(|(i, s)| {
			let stat = stats.iter().find(|st| st.id == s.id);
			let info = stat
				.filter(|st| st.width > 0)
				.map(|st| format!("{}×{} · {:.0} fps", st.width, st.height, st.fps))
				.unwrap_or_default();
			StudioSource {
				id: s.id as i32,
				name: s.label().into(),
				kind: s.kind.label().into(),
				visible: s.visible,
				locked: s.locked,
				background: background_name(&s.background).into(),
				error: stat.and_then(|st| st.error.clone()).unwrap_or_default().into(),
				info: info.into(),
				front: i == last,
				back: i == 0,
			}
		})
		.collect()
}

/// Linear gain as dB (silence: -60).
pub fn gain_db(gain: f32) -> f32 {
	if gain > 0.001 { 20.0 * gain.log10() } else { -60.0 }
}

/// dB as linear gain.
pub fn gain_linear(db: f32) -> f32 {
	10f32.powf(db / 20.0)
}

/// Name, detail and icon kind of an audio source.
pub fn audio_label(kind: &AudioSourceKindSetting) -> (String, String, &'static str) {
	match kind {
		AudioSourceKindSetting::Microphone => ("Microphone".into(), String::new(), "microphone"),
		AudioSourceKindSetting::Desktop => {
			("Desktop Audio".into(), "without Voelin".into(), "desktop")
		}
		AudioSourceKindSetting::App { name: Some(name), .. } => {
			("App Audio".into(), name.clone(), "app")
		}
		AudioSourceKindSetting::App { pid, .. } => {
			("App Audio".into(), pid.map(|p| format!("process {p}")).unwrap_or_default(), "app")
		}
		AudioSourceKindSetting::Window => {
			("Window Audio".into(), "the shared window".into(), "window-audio")
		}
		AudioSourceKindSetting::Synthetic { frequency } => {
			("Test Tone".into(), format!("{frequency} Hz"), "tone")
		}
	}
}

/// What the mixer says about one source now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioLive {
	/// Peak in dBFS.
	pub level: f32,
	pub error: Option<String>,
}

/// The mixer rows: the stream's audio sources with their meters.
pub fn audio(sources: &[AudioSourceSetting], live: &[Option<AudioLive>]) -> Vec<StudioAudio> {
	sources
		.iter()
		.enumerate()
		.map(|(i, s)| {
			let (name, detail, kind) = audio_label(&s.kind);
			let live = live.get(i).cloned().flatten();
			StudioAudio {
				name: name.into(),
				detail: detail.into(),
				kind: kind.into(),
				gain_db: gain_db(s.gain),
				muted: s.muted,
				level: live.as_ref().map_or(-100.0, |l| l.level),
				error: live.and_then(|l| l.error).unwrap_or_default().into(),
			}
		})
		.collect()
}

/// A number without trailing zeros ("0.5", "1").
fn short(value: f32) -> String {
	let text = format!("{value:.3}");
	text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The simulcast layers as rows (bitrates in kbit/s).
pub fn layers(layers: &[LayerSetting]) -> Vec<StudioLayer> {
	layers
		.iter()
		.map(|l| StudioLayer {
			size: l.size.map(|(w, h)| format!("{w}x{h}")).unwrap_or_default().into(),
			scale: short(l.scale).into(),
			fps: l.max_fps.map(|f| f.to_string()).unwrap_or_default().into(),
			bitrate: (l.bitrate / 1000).to_string().into(),
			min_bitrate: (l.min_bitrate / 1000).to_string().into(),
		})
		.collect()
}

/// A layer as typed into `old`: a size ("1280x720") or a scale ("0.5",
/// "0.5×", "50%"), a frame-rate cap (empty: none), kbit/s. Any numbers.
pub fn parse_layer(row: &StudioLayer, old: &LayerSetting) -> Result<LayerSetting, String> {
	let mut layer = old.clone();
	let size = row.size.trim();
	let parts: Vec<&str> = size.split(['x', 'X', '×']).map(str::trim).collect();
	match parts.as_slice() {
		[w, h] if !w.is_empty() && !h.is_empty() => {
			let (w, h) = (w.parse::<u32>(), h.parse::<u32>());
			let (Ok(w), Ok(h)) = (w, h) else { return Err(format!("{size:?} is not a size")) };
			layer.size = Some((w, h));
		}
		_ => {
			let text = size.trim_end_matches(['x', 'X', '×', '%']).trim();
			let mut scale: f32 = text.parse().map_err(|_| format!("{size:?} is not a size"))?;
			if size.ends_with('%') {
				scale /= 100.0;
			}
			layer.size = None;
			layer.scale = scale;
		}
	}
	let fps = row.fps.trim();
	layer.max_fps =
		if fps.is_empty() { None } else { Some(fps.parse().map_err(|_| "a frame rate")?) };
	let kbps: u64 = row.bitrate.trim().parse().map_err(|_| "kbit/s as a number")?;
	layer.bitrate = kbps * 1000;
	Ok(layer)
}

/// A layer below the last one: half its size, a quarter of its bitrate.
pub fn next_layer(layers: &[LayerSetting], bitrate_kbps: u32) -> LayerSetting {
	match layers.last() {
		None => LayerSetting { bitrate: u64::from(bitrate_kbps) * 1000, ..LayerSetting::default() },
		Some(last) => LayerSetting {
			id: None,
			scale: last.size.map_or(last.scale / 2.0, |_| 0.5),
			size: last.size.map(|(w, h)| (w / 2, h / 2)),
			bitrate: (last.bitrate / 4).max(100_000),
			rid: None,
			..last.clone()
		},
	}
}

/// 0: not live, 1 poor … 4 excellent; how the stream gets out. The worse
/// of the frame rate made (against the output's) and the bandwidth the
/// viewers have (their estimates against the bitrate sent).
pub fn quality(
	live: bool,
	wanted_fps: u32,
	fps: f64,
	bandwidth: Option<f64>,
) -> (i32, &'static str) {
	if !live {
		return (0, "Not live");
	}
	let rate = if wanted_fps == 0 { 1.0 } else { fps / f64::from(wanted_fps) };
	let worst = rate.min(bandwidth.unwrap_or(1.0));
	match worst {
		w if w >= 0.9 => (4, "Excellent Connection"),
		w if w >= 0.75 => (3, "Good Connection"),
		w if w >= 0.5 => (2, "Fair Connection"),
		_ => (1, "Poor Connection"),
	}
}

/// 8250 → "8,250".
pub fn thousands(n: u64) -> String {
	let digits = n.to_string();
	let mut out = String::new();
	for (i, c) in digits.chars().enumerate() {
		if i > 0 && (digits.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(c);
	}
	out
}

/// "1080p • 60 FPS • 8,250 kbps".
pub fn format_line(height: u32, fps: u32, kbps: u64) -> String {
	format!("{height}p • {fps} FPS • {} kbps", thousands(kbps))
}

/// "00:12:34".
pub fn clock(d: Duration) -> String {
	let s = d.as_secs();
	format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// "#rrggbb" or "#rrggbbaa" (the # optional).
pub fn parse_colour(text: &str) -> Option<Colour> {
	let hex = text.trim().trim_start_matches('#');
	if !matches!(hex.len(), 6 | 8) || !hex.is_ascii() {
		return None;
	}
	let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
	let a = if hex.len() == 8 { byte(6)? } else { 255 };
	Some(Colour::rgba(byte(0)?, byte(2)?, byte(4)?, a))
}

pub fn colour_text(c: Colour) -> String {
	if c.a == 255 {
		format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
	} else {
		format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
	}
}

fn number(value: f32) -> String {
	short(value)
}

/// The source dialog's form for `source`.
pub fn form(source: &Source) -> StudioSourceForm {
	let t = &source.transform;
	let mut form = StudioSourceForm {
		id: source.id as i32,
		name: source.name.clone().into(),
		kind: source.kind.label().into(),
		x: number(t.x).into(),
		y: number(t.y).into(),
		width: t.width.map(number).unwrap_or_default().into(),
		height: t.height.map(number).unwrap_or_default().into(),
		scale: number(t.scale).into(),
		fit: match t.fit {
			Fit::Contain => "contain",
			Fit::Cover => "cover",
			Fit::Stretch => "stretch",
		}
		.into(),
		crop_left: source.crop.left.to_string().into(),
		crop_top: source.crop.top.to_string().into(),
		crop_right: source.crop.right.to_string().into(),
		crop_bottom: source.crop.bottom.to_string().into(),
		opacity: source.opacity * 100.0,
		locked: source.locked,
		visible: source.visible,
		background: background_name(&source.background).into(),
		blur: "4".into(),
		background_colour: "#00b140".into(),
		..StudioSourceForm::default()
	};
	match &source.background {
		Background::Blur { strength } => form.blur = number(strength * 100.0).into(),
		Background::Image { path } => form.background_path = path.display().to_string().into(),
		Background::Colour { colour } => form.background_colour = colour_text(*colour).into(),
		Background::Keep => {}
	}
	match &source.kind {
		SourceKind::Text { text, size_px, colour, .. } => {
			form.text = text.replace('\n', "\\n").into();
			form.text_size = number(*size_px).into();
			form.colour = colour_text(*colour).into();
		}
		SourceKind::Colour { colour, .. } => form.colour = colour_text(*colour).into(),
		SourceKind::Image { path } => form.path = path.display().to_string().into(),
		SourceKind::Camera { mirror, .. } => form.mirror = *mirror,
		_ => {}
	}
	form
}

fn parse_f32(text: &str, what: &str) -> Result<f32, String> {
	text.trim()
		.parse::<f32>()
		.ok()
		.filter(|v| v.is_finite())
		.ok_or_else(|| format!("{what}: a number"))
}

/// An optional size: empty is `None`.
fn parse_size(text: &str, what: &str) -> Result<Option<f32>, String> {
	if text.trim().is_empty() {
		return Ok(None);
	}
	let v = parse_f32(text, what)?;
	if v > 0.0 { Ok(Some(v)) } else { Err(format!("{what}: above 0")) }
}

fn parse_crop(text: &str) -> Result<u32, String> {
	if text.trim().is_empty() {
		return Ok(0);
	}
	text.trim().parse().map_err(|_| "crop: whole pixels".to_owned())
}

fn transform(form: &StudioSourceForm) -> Result<Transform, String> {
	let scale = if form.scale.trim().is_empty() { 1.0 } else { parse_f32(&form.scale, "scale")? };
	if scale <= 0.0 {
		return Err("scale: above 0".into());
	}
	Ok(Transform {
		x: parse_f32(&form.x, "X").unwrap_or(0.0),
		y: parse_f32(&form.y, "Y").unwrap_or(0.0),
		width: parse_size(&form.width, "width")?,
		height: parse_size(&form.height, "height")?,
		scale,
		fit: match form.fit.as_str() {
			"cover" => Fit::Cover,
			"stretch" => Fit::Stretch,
			_ => Fit::Contain,
		},
	})
}

fn background(form: &StudioSourceForm) -> Result<Background, String> {
	Ok(match form.background.as_str() {
		"blur" => Background::Blur { strength: parse_f32(&form.blur, "blur")?.max(0.0) / 100.0 },
		"image" if !form.background_path.trim().is_empty() => {
			Background::Image { path: form.background_path.trim().into() }
		}
		"image" => return Err("background image: a file".into()),
		"colour" => Background::Colour {
			colour: parse_colour(&form.background_colour).ok_or("background colour: #rrggbb")?,
		},
		_ => Background::Keep,
	})
}

/// What `form` changes in `old` (only what differs).
pub fn change(form: &StudioSourceForm, old: &Source) -> Result<SourceChange, String> {
	let transform = transform(form)?;
	let crop = Crop {
		left: parse_crop(&form.crop_left)?,
		top: parse_crop(&form.crop_top)?,
		right: parse_crop(&form.crop_right)?,
		bottom: parse_crop(&form.crop_bottom)?,
	};
	let kind = kind(form, Some(&old.kind))?;
	let background = background(form)?;
	let opacity = (form.opacity / 100.0).clamp(0.0, 1.0);
	let differs = |a: bool| a.then_some(());
	Ok(SourceChange {
		name: differs(form.name.as_str() != old.name).map(|_| form.name.to_string()),
		kind: differs(kind != old.kind).map(|_| kind),
		transform: differs(transform != old.transform).map(|_| transform),
		crop: differs(crop != old.crop).map(|_| crop),
		opacity: differs((opacity - old.opacity).abs() > f32::EPSILON).map(|_| opacity),
		visible: differs(form.visible != old.visible).map(|_| form.visible),
		locked: differs(form.locked != old.locked).map(|_| form.locked),
		background: differs(background != old.background).map(|_| background),
	})
}

/// The kind as the form says: text, colour and image from their fields, a
/// camera's mirroring, anything else as it was.
fn kind(form: &StudioSourceForm, old: Option<&SourceKind>) -> Result<SourceKind, String> {
	Ok(match (form.kind.as_str(), old) {
		("text", old) => {
			let (font, backdrop, align, padding) = match old {
				Some(SourceKind::Text { font, backdrop, align, padding, .. }) => {
					(font.clone(), *backdrop, *align, *padding)
				}
				_ => (None, Colour::CLEAR, Align::Left, 0),
			};
			let size_px = parse_f32(&form.text_size, "text size")?;
			if size_px <= 0.0 {
				return Err("text size: above 0".into());
			}
			SourceKind::Text {
				text: form.text.replace("\\n", "\n"),
				font,
				size_px,
				colour: parse_colour(&form.colour).ok_or("colour: #rrggbb")?,
				backdrop,
				align,
				padding,
			}
		}
		("colour", old) => SourceKind::Colour {
			colour: parse_colour(&form.colour).ok_or("colour: #rrggbb")?,
			size: match old {
				Some(SourceKind::Colour { size, .. }) => *size,
				_ => (1920, 1080),
			},
		},
		("image", _) if form.path.trim().is_empty() => return Err("image: a file".into()),
		("image", _) => SourceKind::Image { path: form.path.trim().into() },
		(_, Some(SourceKind::Camera { device, size, fps, .. })) => SourceKind::Camera {
			device: device.clone(),
			size: *size,
			fps: *fps,
			mirror: form.mirror,
		},
		(_, Some(old)) => old.clone(),
		(other, None) => return Err(format!("cannot add a {other} source here")),
	})
}

/// A new text, colour or image source from the dialog (id 0: the
/// controller gives it one).
pub fn new_source(form: &StudioSourceForm, output: (u32, u32)) -> Result<Source, String> {
	let kind = kind(form, None)?;
	let mut source = Source::new(0, kind);
	source.name = form.name.trim().to_owned();
	source.transform = if form.kind == "colour" {
		Transform::full(output.0, output.1)
	} else {
		let mut t = placement(&source.kind, output);
		if let Ok(x) = parse_f32(&form.x, "X") {
			t.x = x;
		}
		if let Ok(y) = parse_f32(&form.y, "Y") {
			t.y = y;
		}
		t
	};
	source.opacity = (form.opacity / 100.0).clamp(0.0, 1.0);
	Ok(source)
}

/// Where a new source goes in a `w` x `h` picture: captures fill it
/// (cropping what does not fit), a camera sits in the bottom right corner,
/// text at the bottom left, images at their own size in the top left.
pub fn placement(kind: &SourceKind, (w, h): (u32, u32)) -> Transform {
	let (w, h) = (w as f32, h as f32);
	match kind {
		SourceKind::Camera { .. } => {
			let width = (w * 0.25).round();
			let height = (width * 9.0 / 16.0).round();
			let margin = (h * 0.03).round();
			Transform {
				fit: Fit::Cover,
				..Transform::box_at(w - width - margin, h - height - margin, width, height)
			}
		}
		SourceKind::Text { .. } => {
			Transform { x: (w * 0.03).round(), y: (h * 0.8).round(), ..Transform::default() }
		}
		SourceKind::Image { .. } => Transform::default(),
		_ => Transform { fit: Fit::Cover, ..Transform::box_at(0.0, 0.0, w, h) },
	}
}

/// A source the stream settings' switches keep: its name, and its text
/// (`None`: switched off, so it is removed).
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
	pub name: &'static str,
	pub text: Option<String>,
}

pub const OVERLAY_VIEWERS: &str = "Viewer count";
pub const OVERLAY_CHAT: &str = "Chat overlay";
pub const OVERLAY_NOW_PLAYING: &str = "Now playing";

fn overlay_kind(name: &str, text: &str) -> SourceKind {
	SourceKind::Text {
		text: text.to_owned(),
		font: None,
		size_px: if name == OVERLAY_CHAT { 26.0 } else { 34.0 },
		colour: Colour::WHITE,
		backdrop: Colour::rgba(6, 10, 24, 170),
		align: Align::Left,
		padding: 14,
	}
}

/// Where an overlay starts (people can move it): the viewer count at the
/// top right (by an estimate of its width), the chat at the left, what is
/// on above the bottom left. Clear of the corners the studio's own badges
/// cover in the preview.
fn overlay_place(name: &str, text: &str, (w, h): (u32, u32)) -> Transform {
	let (w, h) = (w as f32, h as f32);
	let margin = (w * 0.025).round();
	let (x, y) = match name {
		OVERLAY_VIEWERS => {
			let wide = text.chars().count() as f32 * 34.0 * 0.58 + 28.0;
			(w - margin - wide, h * 0.14)
		}
		OVERLAY_CHAT => (margin, h * 0.3),
		_ => (margin, h * 0.7),
	};
	Transform { x: x.max(0.0).round(), y: y.round(), ..Transform::default() }
}

/// Make the scenes hold `wanted`: an overlay that is on is added to the live
/// scene if it has none, and its text follows in every scene; one that is
/// off is removed everywhere. Overlays are found by their names, so a
/// person can move them like any source. Returns whether anything changed.
pub fn apply_overlays(scenes: &mut Scenes, wanted: &[Overlay]) -> bool {
	let size = scenes.size();
	let live = scenes.active().map(|s| s.id);
	let mut changed = false;
	for overlay in wanted {
		match &overlay.text {
			None => {
				for scene in &mut scenes.scenes {
					let before = scene.sources.len();
					scene.sources.retain(|s| s.name != overlay.name);
					changed |= scene.sources.len() != before;
				}
			}
			Some(text) => {
				for scene in &mut scenes.scenes {
					for source in scene.sources.iter_mut().filter(|s| s.name == overlay.name) {
						if let SourceKind::Text { text: shown, .. } = &mut source.kind
							&& shown != text
						{
							shown.clone_from(text);
							changed = true;
						}
					}
				}
				let Some(scene) = live.and_then(|id| scenes.scene_mut(id)) else { continue };
				if !scene.sources.iter().any(|s| s.name == overlay.name) {
					let id = scene.next_source_id();
					scene.sources.push(Source {
						name: overlay.name.into(),
						transform: overlay_place(overlay.name, text, size),
						..Source::new(id, overlay_kind(overlay.name, text))
					});
					changed = true;
				}
			}
		}
	}
	changed
}

#[cfg(test)]
mod tests {
	use super::*;

	fn scene_with(names: &[&str]) -> Scenes {
		let mut scene = Scene::new(1, "Main");
		for (i, name) in names.iter().enumerate() {
			let kind = SourceKind::Colour { colour: Colour::BLACK, size: (16, 16) };
			scene.sources.push(Source { name: (*name).into(), ..Source::new(i as u64 + 1, kind) });
		}
		Scenes { scenes: vec![scene, Scene::new(2, "Break")], active: 1, ..Scenes::default() }
	}

	#[test]
	fn rows_show_scenes_and_sources_front_first() {
		let scenes = scene_with(&["Screen", "Camera"]);
		let rows = super::scenes(&scenes);
		assert_eq!(rows.len(), 2);
		assert!(rows[0].live && !rows[1].live);
		assert_eq!(rows[0].detail, "Screen + Camera");
		assert_eq!(rows[1].detail, "Empty");
		let sources = super::sources(scenes.active(), &[]);
		assert_eq!(
			sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
			["Camera", "Screen"]
		);
		assert!(sources[0].front && !sources[0].back && sources[1].back);
	}

	#[test]
	fn numbers_for_the_header() {
		assert_eq!(thousands(8250), "8,250");
		assert_eq!(thousands(1_234_567), "1,234,567");
		assert_eq!(thousands(999), "999");
		assert_eq!(format_line(1080, 60, 8250), "1080p • 60 FPS • 8,250 kbps");
		assert_eq!(clock(Duration::from_secs(12 * 60 + 34)), "00:12:34");
		assert_eq!(clock(Duration::from_secs(3 * 3600 + 5)), "03:00:05");
		assert_eq!(quality(false, 60, 60.0, None).0, 0);
		assert_eq!(quality(true, 60, 59.0, None).0, 4);
		assert_eq!(quality(true, 60, 59.0, Some(0.6)).0, 2);
		assert_eq!(quality(true, 60, 20.0, Some(1.0)).0, 1);
	}

	#[test]
	fn gains_and_colours_round_trip() {
		assert!((gain_db(1.0)).abs() < 1e-6);
		assert!((gain_db(gain_linear(-6.0)) + 6.0).abs() < 1e-4);
		assert_eq!(gain_db(0.0), -60.0);
		let c = parse_colour("#ff8000").unwrap();
		assert_eq!(c, Colour::rgb(255, 128, 0));
		assert_eq!(colour_text(c), "#ff8000");
		assert_eq!(parse_colour("10203040"), Some(Colour::rgba(16, 32, 48, 64)));
		assert_eq!(parse_colour("#ff80"), None);
		assert_eq!(parse_colour("#gg0000"), None);
	}

	#[test]
	fn layers_as_typed() {
		let old = LayerSetting::default();
		let row = |size: &str, fps: &str, kbps: &str| StudioLayer {
			size: size.into(),
			fps: fps.into(),
			bitrate: kbps.into(),
			..StudioLayer::default()
		};
		let l = parse_layer(&row("1280x720", "30", "2500"), &old).unwrap();
		assert_eq!((l.size, l.max_fps, l.bitrate), (Some((1280, 720)), Some(30), 2_500_000));
		let l = parse_layer(&row("0.5×", "", "800"), &old).unwrap();
		assert_eq!((l.size, l.scale, l.max_fps), (None, 0.5, None));
		assert_eq!(parse_layer(&row("50%", "", "800"), &old).unwrap().scale, 0.5);
		assert!(parse_layer(&row("big", "", "800"), &old).is_err());
		assert!(parse_layer(&row("1", "", "fast"), &old).is_err());
		let rows = layers(&[l]);
		assert_eq!((rows[0].scale.as_str(), rows[0].bitrate.as_str()), ("0.5", "800"));
		let below = next_layer(&[LayerSetting { bitrate: 8_000_000, ..old.clone() }], 8000);
		assert_eq!((below.scale, below.bitrate), (0.5, 2_000_000));
	}

	#[test]
	fn the_source_form_changes_only_what_differs() {
		let mut source = Source::new(
			3,
			SourceKind::Text {
				text: "Hello\nthere".into(),
				font: None,
				size_px: 48.0,
				colour: Colour::WHITE,
				backdrop: Colour::CLEAR,
				align: Align::Center,
				padding: 4,
			},
		);
		source.transform = Transform::box_at(10.0, 20.0, 300.0, 100.0);
		let mut f = form(&source);
		assert_eq!(f.text, "Hello\\nthere");
		assert_eq!((f.x.as_str(), f.width.as_str()), ("10", "300"));
		assert_eq!(change(&f, &source).unwrap(), SourceChange::default());
		f.x = "40".into();
		f.opacity = 50.0;
		f.text = "Bye".into();
		let c = change(&f, &source).unwrap();
		assert_eq!(c.transform.unwrap().x, 40.0);
		assert_eq!(c.opacity, Some(0.5));
		let Some(SourceKind::Text { text, align, padding, .. }) = c.kind else { panic!() };
		assert_eq!((text.as_str(), align, padding), ("Bye", Align::Center, 4));
		f.width = "-1".into();
		assert!(change(&f, &source).is_err());
	}

	#[test]
	fn new_sources_are_placed_by_kind() {
		let camera =
			SourceKind::Camera { device: String::new(), size: None, fps: None, mirror: false };
		let t = placement(&camera, (1920, 1080));
		assert_eq!((t.width, t.height, t.fit), (Some(480.0), Some(270.0), Fit::Cover));
		assert!(t.x + 480.0 < 1920.0 && t.y + 270.0 < 1080.0);
		let screen = SourceKind::Screen { monitor: 0, backend: None, cursor: true };
		assert_eq!(placement(&screen, (1920, 1080)).width, Some(1920.0));
		let form = StudioSourceForm {
			kind: "colour".into(),
			name: "Slate".into(),
			colour: "#101a3c".into(),
			opacity: 100.0,
			..Default::default()
		};
		let s = new_source(&form, (1280, 720)).unwrap();
		assert_eq!((s.name.as_str(), s.transform.width), ("Slate", Some(1280.0)));
		assert!(new_source(&StudioSourceForm { kind: "image".into(), ..form }, (1, 1)).is_err());
	}

	#[test]
	fn overlays_follow_the_switches() {
		let mut scenes = scene_with(&["Screen"]);
		let on = |text: &str| Overlay { name: OVERLAY_VIEWERS, text: Some(text.into()) };
		assert!(apply_overlays(&mut scenes, &[on("3 watching")]));
		let live = &scenes.scenes[0];
		assert_eq!(live.sources.len(), 2);
		assert_eq!(live.sources[1].name, OVERLAY_VIEWERS);
		// Nothing to do the second time; the text follows.
		assert!(!apply_overlays(&mut scenes, &[on("3 watching")]));
		assert!(apply_overlays(&mut scenes, &[on("4 watching")]));
		let SourceKind::Text { text, .. } = &scenes.scenes[0].sources[1].kind else { panic!() };
		assert_eq!(text, "4 watching");
		// A scene switch adds it to the new live scene too; off removes all.
		scenes.active = 2;
		assert!(apply_overlays(&mut scenes, &[on("4 watching")]));
		assert_eq!(scenes.scenes[1].sources.len(), 1);
		assert!(apply_overlays(&mut scenes, &[Overlay { name: OVERLAY_VIEWERS, text: None }]));
		assert_eq!(scenes.scenes[0].sources.len(), 1);
		assert!(scenes.scenes[1].sources.is_empty());
		assert!(scenes.check().is_ok());
	}
}
