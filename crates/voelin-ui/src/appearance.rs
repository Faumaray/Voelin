//! Appearance settings (`ui.theme`, `ui.font_scale`, `ui.narrow_breakpoint`,
//! `ui.image_cache_mb`): applied to the Theme and Nav globals and the image
//! cache whenever they change, from the settings page or elsewhere.

use slint::ComponentHandle;
use tracing::warn;

use crate::app::{App, AppearanceForm, Bridge, Nav, Theme};
use crate::images;
use crate::settings::{
	ThemeChoice, UI_FONT_SCALE, UI_IMAGE_CACHE_MB, UI_NARROW_BREAKPOINT, UI_THEME,
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
		ui.global::<Nav>().set_narrow_breakpoint(breakpoint as f32);
		let bridge = ui.global::<Bridge>();
		bridge.set_appearance(AppearanceForm {
			theme: theme.as_str().into(),
			font_scale: scale,
			narrow_breakpoint: breakpoint as f32,
			image_cache_mb: cache_mb as f32,
		});
		bridge.set_image_cache_usage(images::usage_text().into());
		self.studio_theme();
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
}
