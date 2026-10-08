//! Appearance settings (`ui.theme`, `ui.font_scale`, `ui.narrow_breakpoint`,
//! `ui.image_cache_mb`, `ui.members_width`, `ui.voice_compact`): applied to
//! the Theme and Nav globals and the image cache whenever they change, from
//! the settings page or elsewhere.

use slint::ComponentHandle;
use tracing::warn;

use crate::app::{App, AppearanceForm, Bridge, Nav, Theme};
use crate::images;
use crate::settings::{
	ThemeChoice, UI_FONT_SCALE, UI_IMAGE_CACHE_MB, UI_MEMBERS_WIDTH, UI_NARROW_BREAKPOINT,
	UI_THEME, UI_VOICE_COMPACT,
};

impl App {
	/// Show the current values of the appearance keys.
	pub(crate) fn apply_appearance(&self) {
		let theme = self.prefs.get(&UI_THEME);
		let scale = self.prefs.get(&UI_FONT_SCALE);
		let breakpoint = self.prefs.get(&UI_NARROW_BREAKPOINT);
		let cache_mb = self.prefs.get(&UI_IMAGE_CACHE_MB);
		images::set_budget_mb(cache_mb);
		let Some(ui) = self.ui.upgrade() else { return };
		let t = ui.global::<Theme>();
		t.set_mode(theme.as_str().into());
		t.set_font_scale(scale);
		let nav = ui.global::<Nav>();
		nav.set_narrow_breakpoint(breakpoint as f32);
		let width = self.prefs.get(&UI_MEMBERS_WIDTH) as f32;
		if nav.get_right_panel_width() != width {
			nav.set_right_panel_width(width);
		}
		nav.set_voice_compact(self.prefs.get(&UI_VOICE_COMPACT));
		let bridge = ui.global::<Bridge>();
		bridge.set_appearance(AppearanceForm {
			theme: theme.as_str().into(),
			font_scale: scale,
			narrow_breakpoint: breakpoint as f32,
			image_cache_mb: cache_mb as f32,
		});
		bridge.set_image_cache_usage(images::usage_text().into());
		self.theme_windows();
	}

	/// The windows of their own (the studio's, the stream's) follow the main
	/// window's theme.
	pub(crate) fn theme_windows(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let theme = ui.global::<Theme>();
		if let Some(window) = &self.studio.window {
			copy_theme(&theme, &window.global::<Theme>());
		}
		if let Some(window) = &self.popout.window {
			copy_theme(&theme, &window.global::<Theme>());
		}
	}

	/// The settings page changed the form: store the keys (they apply
	/// through the change events).
	pub(crate) fn appearance_changed(&mut self, form: &AppearanceForm) {
		let results = [
			self.prefs.set(&UI_THEME, ThemeChoice::parse(&form.theme)),
			self.prefs.set(&UI_FONT_SCALE, form.font_scale),
			self.prefs.set(&UI_NARROW_BREAKPOINT, form.narrow_breakpoint.max(0.0).round() as u32),
			self.prefs.set(&UI_IMAGE_CACHE_MB, form.image_cache_mb.max(0.0).round() as u32),
		];
		for result in results {
			if let Err(e) = result {
				warn!(%e, "appearance setting rejected");
				self.set_status(e.to_string());
			}
		}
		// Applied now too: change events come through the engine, which
		// may not run the settings service (tests, a hosted engine).
		self.apply_appearance();
	}

	/// The members panel was dragged to another width: store it, so it
	/// comes back the same and `--set ui.members_width` can change it.
	pub(crate) fn panel_resized(&mut self, width: f32) {
		let width = width.max(0.0).round() as u32;
		if self.prefs.get(&UI_MEMBERS_WIDTH) == width {
			return;
		}
		if let Err(e) = self.prefs.set(&UI_MEMBERS_WIDTH, width) {
			warn!(%e, "could not store the panel width");
		}
	}

	/// The voice channel view's stage was made smaller or larger: store it
	/// (`ui.voice_compact`), so it comes back the same.
	pub(crate) fn voice_compact_changed(&mut self, compact: bool) {
		if self.prefs.get(&UI_VOICE_COMPACT) == compact {
			return;
		}
		if let Err(e) = self.prefs.set(&UI_VOICE_COMPACT, compact) {
			warn!(%e, "could not store the stage size");
		}
	}
}

/// Each window has its own Theme global.
fn copy_theme(from: &Theme, to: &Theme) {
	to.set_mode(from.get_mode());
	to.set_font_scale(from.get_font_scale());
	to.set_system_dark(from.get_system_dark());
}
