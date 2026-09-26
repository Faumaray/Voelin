//! Third-party notices for the About page.
//!
//! `THIRD_PARTY_NOTICES.md` at the repository root is generated from
//! `Cargo.lock` by `scripts/notices.sh` (cargo-about) and embedded here; CI
//! fails when it is stale. It is Markdown: an introduction (Slint, OpenH264,
//! tsclientlib, native libraries), then each license text in a fenced block
//! with the crates that use it. About 500 KB, so show it in a scrolling view
//! (or write it to a file and open that), not in a single label.

/// The full notices, Markdown.
pub fn text() -> &'static str {
	include_str!("../../../THIRD_PARTY_NOTICES.md")
}

/// Attribution the OpenH264 license asks for wherever H.264 can be enabled
/// (the same text as `tsc_media::codec::h264::OPENH264_ATTRIBUTION`).
pub const OPENH264_ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn notices_cover_required_attributions() {
		let text = text();
		assert!(text.starts_with("# Third-party notices"));
		// Slint's royalty-free license text, OpenH264 and the vendored
		// protocol library must be credited.
		assert!(text.contains("Slint Royalty-free Desktop, Mobile, and Web Applications License"));
		assert!(text.contains(OPENH264_ATTRIBUTION));
		assert!(text.contains("tsclientlib contributors"));
		// Bundled C libraries.
		assert!(text.contains("Xiph.Org"));
		assert!(text.contains("The WebM Project authors"));
	}
}
