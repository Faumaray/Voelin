//! The scene graph: scenes, their sources and how each source is placed.
//!
//! This is the persisted shape of a studio ([`Scenes`] is the value of the
//! `studio.scenes` setting), so every type here is serde-serialisable and
//! every field has a default: an older settings file keeps working when a
//! field is added. Nothing here captures, decodes or draws anything; the
//! compositor ([`crate::studio::compose`]) reads it and the sources
//! ([`crate::studio::source`]) do the work.
//!
//! There are no limits: any number of scenes, any number of sources per
//! scene.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::capture::SourceId;

/// Straight (non-premultiplied) 8-bit colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colour {
	pub r: u8,
	pub g: u8,
	pub b: u8,
	/// 255: opaque.
	#[serde(default = "opaque")]
	pub a: u8,
}

fn opaque() -> u8 {
	255
}

impl Default for Colour {
	fn default() -> Self {
		Self::BLACK
	}
}

impl Colour {
	pub const BLACK: Self = Self::rgb(0, 0, 0);
	pub const WHITE: Self = Self::rgb(255, 255, 255);
	/// Fully transparent.
	pub const CLEAR: Self = Self { r: 0, g: 0, b: 0, a: 0 };

	pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
		Self { r, g, b, a: 255 }
	}

	pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
		Self { r, g, b, a }
	}

	/// The colour as RGBA bytes.
	pub const fn to_rgba(self) -> [u8; 4] {
		[self.r, self.g, self.b, self.a]
	}
}

/// How a source fills the box its [`Transform`] gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
	/// Fill the box, changing the aspect ratio.
	Stretch,
	/// The whole source inside the box, keeping its aspect ratio (letterbox).
	#[default]
	Contain,
	/// Fill the box keeping the aspect ratio, cropping what does not fit.
	Cover,
}

/// Where and how large a source is drawn, in output pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Transform {
	/// Top-left corner in output pixels; may be negative (drawn clipped).
	pub x: f32,
	pub y: f32,
	/// The box to draw into. `None`: the source's own size (after the crop)
	/// times [`scale`](Self::scale).
	pub width: Option<f32>,
	pub height: Option<f32>,
	/// Factor on the source's own size, used where `width` / `height` are
	/// `None`.
	pub scale: f32,
	pub fit: Fit,
}

impl Default for Transform {
	fn default() -> Self {
		Self { x: 0.0, y: 0.0, width: None, height: None, scale: 1.0, fit: Fit::default() }
	}
}

impl Transform {
	/// A box covering the whole `width` x `height` output.
	pub fn full(width: u32, height: u32) -> Self {
		Self::box_at(0.0, 0.0, width as f32, height as f32)
	}

	/// A box at `(x, y)` of `width` x `height` output pixels.
	pub fn box_at(x: f32, y: f32, width: f32, height: f32) -> Self {
		Self { x, y, width: Some(width), height: Some(height), ..Self::default() }
	}
}

/// Pixels cut off each side of the source before it is placed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Crop {
	pub left: u32,
	pub top: u32,
	pub right: u32,
	pub bottom: u32,
}

impl Crop {
	pub fn is_none(&self) -> bool {
		*self == Self::default()
	}

	/// The cropped rectangle `(x, y, width, height)` of a `width` x `height`
	/// source. Never empty: a crop that would leave nothing keeps one pixel.
	pub fn apply(&self, width: u32, height: u32) -> (u32, u32, u32, u32) {
		let x = self.left.min(width.saturating_sub(1));
		let y = self.top.min(height.saturating_sub(1));
		let w = width.saturating_sub(x + self.right).max(1).min(width - x);
		let h = height.saturating_sub(y + self.bottom).max(1).min(height - y);
		(x, y, w, h)
	}
}

/// What replaces the background of a camera or screen source; see
/// [`crate::studio::segment`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Background {
	/// Leave the frame alone (no segmentation runs).
	#[default]
	Keep,
	/// Blur it. `strength` is the blur radius as a fraction of the frame
	/// height (0.02 is a gentle blur, 0.1 a strong one).
	Blur {
		#[serde(default = "default_blur")]
		strength: f32,
	},
	/// Replace it with an image, scaled to cover the frame.
	Image { path: PathBuf },
	/// Replace it with a colour.
	Colour { colour: Colour },
}

fn default_blur() -> f32 {
	0.04
}

impl Background {
	/// Whether this needs a person mask (and so the segmenter).
	pub fn needs_mask(&self) -> bool {
		!matches!(self, Self::Keep)
	}
}

/// How text is aligned in its box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
	#[default]
	Left,
	Center,
	Right,
}

/// What a source shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceKind {
	/// A monitor, through one of the screen capture backends.
	Screen {
		#[serde(default)]
		monitor: u32,
		/// `portal`, `wlroots`, `x11`, `windows`; `None`: the best one for
		/// this session.
		#[serde(default)]
		backend: Option<String>,
		#[serde(default = "yes")]
		cursor: bool,
	},
	/// A window by its native handle (X11 window id, Windows `HWND`).
	Window {
		handle: u64,
		#[serde(default)]
		backend: Option<String>,
		#[serde(default = "yes")]
		cursor: bool,
	},
	/// Whatever the desktop's ScreenCast portal dialog picks (Wayland).
	Portal {
		/// Token of an earlier choice: the desktop may skip its dialog.
		#[serde(default)]
		restore_token: Option<String>,
		#[serde(default = "yes")]
		cursor: bool,
	},
	/// A camera. `device` is a [`crate::studio::camera::Camera::id`]; empty:
	/// the first camera found.
	Camera {
		#[serde(default)]
		device: String,
		/// Wanted capture size; `None`: the camera's default.
		#[serde(default)]
		size: Option<(u32, u32)>,
		#[serde(default)]
		fps: Option<u32>,
		/// Mirror the picture (what people expect of a front camera).
		#[serde(default)]
		mirror: bool,
	},
	/// A still image (PNG, JPEG, ...), with its alpha channel.
	Image { path: PathBuf },
	/// Text rasterised with a TrueType font.
	Text {
		text: String,
		/// A `.ttf` / `.otf` file; `None`: the bundled UI font.
		#[serde(default)]
		font: Option<PathBuf>,
		#[serde(default = "default_text_size")]
		size_px: f32,
		#[serde(default = "white")]
		colour: Colour,
		/// Filled behind the glyphs (transparent by default). Not named
		/// `background`: [`Source::background`] is flattened into the same
		/// object.
		#[serde(default = "clear")]
		backdrop: Colour,
		#[serde(default)]
		align: Align,
		/// Pixels between the text and the edge of the source.
		#[serde(default)]
		padding: u32,
	},
	/// A flat colour (a background, a divider, a placeholder).
	Colour {
		colour: Colour,
		/// Its own size in pixels before the transform.
		#[serde(default = "colour_size")]
		size: (u32, u32),
	},
	/// The synthetic test pattern (tests and previews).
	Pattern {
		#[serde(default = "pattern_size")]
		size: (u32, u32),
	},
}

fn yes() -> bool {
	true
}

fn default_text_size() -> f32 {
	48.0
}

fn white() -> Colour {
	Colour::WHITE
}

fn clear() -> Colour {
	Colour::CLEAR
}

fn colour_size() -> (u32, u32) {
	(1920, 1080)
}

fn pattern_size() -> (u32, u32) {
	(1280, 720)
}

impl SourceKind {
	/// A short name for the kind, for logs and the UI.
	pub fn label(&self) -> &'static str {
		match self {
			Self::Screen { .. } => "screen",
			Self::Window { .. } => "window",
			Self::Portal { .. } => "portal",
			Self::Camera { .. } => "camera",
			Self::Image { .. } => "image",
			Self::Text { .. } => "text",
			Self::Colour { .. } => "colour",
			Self::Pattern { .. } => "pattern",
		}
	}

	/// The capture source this kind asks a [`crate::ScreenCapture`] backend
	/// for, if it is a screen capture at all.
	pub fn capture_source(&self) -> Option<SourceId> {
		match self {
			Self::Screen { monitor, .. } => Some(SourceId::Monitor(*monitor)),
			Self::Window { handle, .. } => Some(SourceId::Window(*handle)),
			Self::Portal { .. } => Some(SourceId::Portal),
			Self::Pattern { .. } => Some(SourceId::Synthetic),
			_ => None,
		}
	}

	/// Whether a source that changes from one kind to the other keeps its
	/// running input. Only what is drawn differs for a camera's mirroring,
	/// and a portal's restore token is what the portal wrote back, not a
	/// reason to ask the user again; any other change starts the input anew
	/// (for text and colour that is just drawing them again).
	pub fn same_input(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::Portal { cursor: a, .. }, Self::Portal { cursor: b, .. }) => a == b,
			(
				Self::Camera { device: a, size: sa, fps: fa, .. },
				Self::Camera { device: b, size: sb, fps: fb, .. },
			) => (a, sa, fa) == (b, sb, fb),
			_ => self == other,
		}
	}
}

/// One source of a scene.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
	/// Stable within its scene, for the UI and the controller API.
	pub id: u64,
	/// Shown in the source list; empty: [`SourceKind::label`].
	#[serde(default)]
	pub name: String,
	#[serde(flatten)]
	pub kind: SourceKind,
	#[serde(default)]
	pub transform: Transform,
	#[serde(default)]
	pub crop: Crop,
	/// 0: invisible, 1: opaque.
	#[serde(default = "unity")]
	pub opacity: f32,
	#[serde(default = "yes")]
	pub visible: bool,
	/// The UI refuses to move or resize a locked source.
	#[serde(default)]
	pub locked: bool,
	/// Background replacement (cameras and screens).
	#[serde(default)]
	pub background: Background,
}

fn unity() -> f32 {
	1.0
}

impl Source {
	/// A source of `kind` with id `id` and everything else default.
	pub fn new(id: u64, kind: SourceKind) -> Self {
		Self {
			id,
			name: String::new(),
			kind,
			transform: Transform::default(),
			crop: Crop::default(),
			opacity: 1.0,
			visible: true,
			locked: false,
			background: Background::default(),
		}
	}

	/// Its name, or the kind's label.
	pub fn label(&self) -> &str {
		if self.name.is_empty() { self.kind.label() } else { &self.name }
	}

	/// Whether it contributes anything to the composite.
	pub fn drawn(&self) -> bool {
		self.visible && self.opacity > 0.0
	}
}

/// A scene: sources drawn back to front over a background.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
	pub id: u64,
	#[serde(default)]
	pub name: String,
	/// Drawn in order, so the last one is on top.
	#[serde(default)]
	pub sources: Vec<Source>,
	/// Filled before the sources.
	#[serde(default)]
	pub background: Colour,
}

impl Scene {
	pub fn new(id: u64, name: impl Into<String>) -> Self {
		Self { id, name: name.into(), sources: Vec::new(), background: Colour::BLACK }
	}

	pub fn source(&self, id: u64) -> Option<&Source> {
		self.sources.iter().find(|s| s.id == id)
	}

	pub fn source_mut(&mut self, id: u64) -> Option<&mut Source> {
		self.sources.iter_mut().find(|s| s.id == id)
	}

	/// The next free source id.
	pub fn next_source_id(&self) -> u64 {
		self.sources.iter().map(|s| s.id + 1).max().unwrap_or(1)
	}
}

/// Every scene of a studio and which one is live. The value of the
/// `studio.scenes` setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scenes {
	pub scenes: Vec<Scene>,
	/// The scene being composited; ignored if no scene has this id.
	pub active: u64,
	/// Output size of the composite.
	pub width: u32,
	pub height: u32,
	pub fps: u32,
}

impl Default for Scenes {
	fn default() -> Self {
		Self { scenes: Vec::new(), active: 0, width: 1920, height: 1080, fps: 30 }
	}
}

impl Scenes {
	/// The live scene: the one with id [`active`](Self::active), else the
	/// first.
	pub fn active(&self) -> Option<&Scene> {
		self.scenes.iter().find(|s| s.id == self.active).or_else(|| self.scenes.first())
	}

	pub fn active_mut(&mut self) -> Option<&mut Scene> {
		let id = self.active().map(|s| s.id)?;
		self.scenes.iter_mut().find(|s| s.id == id)
	}

	pub fn scene(&self, id: u64) -> Option<&Scene> {
		self.scenes.iter().find(|s| s.id == id)
	}

	pub fn scene_mut(&mut self, id: u64) -> Option<&mut Scene> {
		self.scenes.iter_mut().find(|s| s.id == id)
	}

	/// The next free scene id.
	pub fn next_scene_id(&self) -> u64 {
		self.scenes.iter().map(|s| s.id + 1).max().unwrap_or(1)
	}

	/// Output size, at least 2x2 and even (the encoders want even sizes).
	pub fn size(&self) -> (u32, u32) {
		(self.width.max(2) & !1, self.height.max(2) & !1)
	}

	pub fn fps(&self) -> u32 {
		self.fps.max(1)
	}

	/// What is wrong with these scenes, if anything. Duplicate ids are the
	/// one thing the rest of the studio cannot cope with.
	pub fn check(&self) -> Result<(), String> {
		for (i, scene) in self.scenes.iter().enumerate() {
			if self.scenes[..i].iter().any(|s| s.id == scene.id) {
				return Err(format!("scene id {} appears twice", scene.id));
			}
			for (j, source) in scene.sources.iter().enumerate() {
				if scene.sources[..j].iter().any(|s| s.id == source.id) {
					return Err(format!(
						"scene {}: source id {} appears twice",
						scene.id, source.id
					));
				}
				if !(source.opacity.is_finite() && (0.0..=1.0).contains(&source.opacity)) {
					return Err(format!(
						"scene {} source {}: opacity must be between 0 and 1",
						scene.id, source.id
					));
				}
				let t = &source.transform;
				if !(t.x.is_finite() && t.y.is_finite() && t.scale.is_finite() && t.scale > 0.0) {
					return Err(format!(
						"scene {} source {}: position must be finite and scale above 0",
						scene.id, source.id
					));
				}
				if [t.width, t.height].iter().flatten().any(|v| !(v.is_finite() && *v > 0.0)) {
					return Err(format!(
						"scene {} source {}: width and height must be finite and above 0",
						scene.id, source.id
					));
				}
				if let Background::Blur { strength } = &source.background
					&& !(strength.is_finite() && *strength >= 0.0)
				{
					return Err(format!(
						"scene {} source {}: blur strength must be a number >= 0",
						scene.id, source.id
					));
				}
			}
		}
		if self.width == 0 || self.height == 0 {
			return Err("the output size must not be zero".into());
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn serde_round_trip_and_defaults() {
		let mut scenes = Scenes { width: 1280, height: 720, fps: 60, ..Scenes::default() };
		let mut main = Scene::new(1, "Main");
		main.sources.push(Source {
			transform: Transform::full(1280, 720),
			..Source::new(1, SourceKind::Screen { monitor: 0, backend: None, cursor: true })
		});
		main.sources.push(Source {
			name: "Cam".into(),
			transform: Transform::box_at(960.0, 540.0, 320.0, 180.0),
			crop: Crop { left: 10, top: 0, right: 10, bottom: 0 },
			opacity: 0.8,
			background: Background::Blur { strength: 0.05 },
			..Source::new(
				2,
				SourceKind::Camera {
					device: "cam0".into(),
					size: Some((1280, 720)),
					fps: Some(30),
					mirror: true,
				},
			)
		});
		main.sources.push(Source::new(
			3,
			SourceKind::Text {
				text: "LIVE".into(),
				font: None,
				size_px: 64.0,
				colour: Colour::rgb(255, 64, 64),
				backdrop: Colour::CLEAR,
				align: Align::Center,
				padding: 8,
			},
		));
		scenes.scenes.push(main);
		scenes.scenes.push(Scene::new(2, "Break"));
		scenes.active = 2;
		scenes.check().unwrap();

		let json = serde_json::to_string(&scenes).unwrap();
		assert_eq!(serde_json::from_str::<Scenes>(&json).unwrap(), scenes);
		// The kind is flattened into the source, so a scene file reads well.
		assert!(json.contains(r#""kind":"camera""#), "{json}");

		// Everything but the ids and the kind has a default.
		let terse: Scenes = serde_json::from_str(
			r#"{"scenes":[{"id":7,"sources":[{"id":1,"kind":"colour",
			   "colour":{"r":1,"g":2,"b":3}}]}]}"#,
		)
		.unwrap();
		let source = &terse.scenes[0].sources[0];
		assert_eq!(source.opacity, 1.0);
		assert!(source.visible && !source.locked);
		assert_eq!(source.transform, Transform::default());
		assert_eq!(source.background, Background::Keep);
		assert_eq!(
			source.kind,
			SourceKind::Colour { colour: Colour::rgb(1, 2, 3), size: (1920, 1080) }
		);
		// No active id: the first scene is live.
		assert_eq!(terse.active().map(|s| s.id), Some(7));
		assert_eq!(terse.size(), (1920, 1080));
	}

	#[test]
	fn check_rejects_duplicates_and_nonsense() {
		let mut scenes = Scenes::default();
		scenes.scenes.push(Scene::new(1, "a"));
		scenes.scenes.push(Scene::new(1, "b"));
		assert!(scenes.check().unwrap_err().contains("scene id 1"));
		scenes.scenes[1].id = 2;
		scenes.check().unwrap();
		assert_eq!(scenes.next_scene_id(), 3);

		let scene = &mut scenes.scenes[0];
		scene
			.sources
			.push(Source::new(5, SourceKind::Colour { colour: Colour::BLACK, size: (4, 4) }));
		scene.sources.push(Source::new(5, SourceKind::Pattern { size: (4, 4) }));
		assert!(scenes.check().unwrap_err().contains("source id 5"));
		scenes.scenes[0].sources[1].id = 6;
		assert_eq!(scenes.scenes[0].next_source_id(), 7);
		scenes.scenes[0].sources[0].opacity = 1.5;
		assert!(scenes.check().unwrap_err().contains("opacity"));
		scenes.scenes[0].sources[0].opacity = 1.0;
		scenes.scenes[0].sources[0].transform.scale = 0.0;
		assert!(scenes.check().unwrap_err().contains("scale"));
		scenes.scenes[0].sources[0].transform =
			Transform { width: Some(f32::INFINITY), ..Transform::default() };
		assert!(scenes.check().unwrap_err().contains("width and height"));
	}

	#[test]
	fn which_changes_keep_the_input() {
		let camera = |mirror, fps| SourceKind::Camera {
			device: "/dev/video0".into(),
			size: None,
			fps: Some(fps),
			mirror,
		};
		assert!(camera(false, 30).same_input(&camera(true, 30)));
		assert!(!camera(false, 30).same_input(&camera(false, 60)));
		let portal = |token: Option<&str>| SourceKind::Portal {
			restore_token: token.map(str::to_owned),
			cursor: true,
		};
		assert!(portal(None).same_input(&portal(Some("t"))));
		let text = |text: &str| SourceKind::Text {
			text: text.into(),
			font: None,
			size_px: 48.0,
			colour: Colour::WHITE,
			backdrop: Colour::CLEAR,
			align: Align::Left,
			padding: 0,
		};
		assert!(!text("a").same_input(&text("b")), "new text is drawn anew");
	}

	#[test]
	fn crop_and_labels() {
		assert_eq!(Crop::default().apply(100, 50), (0, 0, 100, 50));
		assert_eq!(Crop { left: 10, top: 5, right: 20, bottom: 5 }.apply(100, 50), (10, 5, 70, 40));
		// A crop that leaves nothing keeps one pixel.
		assert_eq!(Crop { left: 60, top: 0, right: 60, bottom: 0 }.apply(100, 50), (60, 0, 1, 50));
		assert_eq!(Crop { left: 200, top: 0, right: 0, bottom: 0 }.apply(100, 50), (99, 0, 1, 50));
		assert!(Crop::default().is_none());

		let source = Source::new(1, SourceKind::Pattern { size: (2, 2) });
		assert_eq!(source.label(), "pattern");
		assert_eq!(source.kind.capture_source(), Some(SourceId::Synthetic));
		assert!(source.drawn());
		assert!(!Source { opacity: 0.0, ..source.clone() }.drawn());
		assert!(!Source { visible: false, ..source }.drawn());
		assert_eq!(
			SourceKind::Window { handle: 9, backend: None, cursor: false }.capture_source(),
			Some(SourceId::Window(9))
		);
		assert!(SourceKind::Image { path: "a.png".into() }.capture_source().is_none());
	}
}
