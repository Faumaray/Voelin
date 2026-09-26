//! Desktop notifications through notify-rust: the freedesktop notification
//! service over D-Bus on Linux (works from Flatpak too), toasts on Windows.

use std::time::Duration;

use crate::{APP_NAME, Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
	pub title: String,
	pub body: String,
	/// How long it stays; `None` for the desktop's default.
	pub timeout: Option<Duration>,
	/// Urgent (a poke, a private message): kept until dismissed where the
	/// desktop supports that.
	pub urgent: bool,
}

impl Notification {
	pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
		Self { title: title.into(), body: body.into(), ..Default::default() }
	}
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn native(n: &Notification) -> notify_rust::Notification {
	let mut native = notify_rust::Notification::new();
	native.summary(&n.title).body(&n.body).appname(APP_NAME);
	if let Some(timeout) = n.timeout {
		native.timeout(notify_rust::Timeout::Milliseconds(timeout.as_millis() as u32));
	}
	#[cfg(all(unix, not(target_os = "macos")))]
	{
		// The desktop file and icon are installed under the app id.
		native.icon(crate::APP_ID).hint(notify_rust::Hint::DesktopEntry(crate::APP_ID.into()));
		if n.urgent {
			native.urgency(notify_rust::Urgency::Critical);
		}
	}
	native
}

/// Show a notification. Fails when there is no notification service (e.g. no
/// D-Bus session); callers usually just log that.
pub async fn notify(n: &Notification) -> Result<()> {
	#[cfg(all(unix, not(target_os = "macos")))]
	{
		native(n).show_async().await.map_err(|e| Error::Notification(e.to_string()))?;
		Ok(())
	}
	#[cfg(windows)]
	{
		native(n).show().map_err(|e| Error::Notification(e.to_string()))?;
		Ok(())
	}
	#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
	{
		let _ = (n, APP_NAME);
		Err(Error::Unsupported("notifications".into()))
	}
}

// On Windows a toast would really show, so Linux only.
#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
	use super::*;

	/// Without a notification service this must fail, not hang or panic.
	#[tokio::test]
	async fn notify_returns() {
		let n = Notification { urgent: true, ..Notification::new("Poke", "from a test") };
		let result = tokio::time::timeout(Duration::from_secs(10), notify(&n)).await;
		assert!(result.is_ok(), "notify hung");
	}
}
