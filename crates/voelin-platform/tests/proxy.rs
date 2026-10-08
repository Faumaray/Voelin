//! The desktop's proxy through a stand-in xdg-desktop-portal on the
//! session bus. Run it on a private bus:
//! `dbus-run-session -- cargo test -p voelin-platform --test proxy`;
//! without a session bus it checks nothing. Its own test binary: answers
//! are kept process-wide.

#![cfg(all(unix, not(target_os = "macos"), feature = "portal"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use voelin_platform::proxy::proxy_for;

/// What the GNOME portal answers: a SOCKS proxy for one host, none for the
/// others.
struct Resolver {
	lookups: Arc<AtomicU32>,
}

#[zbus::interface(name = "org.freedesktop.portal.ProxyResolver")]
impl Resolver {
	fn lookup(&self, uri: &str) -> Vec<String> {
		self.lookups.fetch_add(1, Ordering::SeqCst);
		if uri.starts_with("https://capped.example") {
			vec!["socks://127.0.0.1:2080".into()]
		} else {
			vec!["direct://".into()]
		}
	}

	#[zbus(property)]
	fn version(&self) -> u32 {
		1
	}
}

#[tokio::test]
async fn the_portal_names_the_proxy() {
	if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
		eprintln!("no session bus: run under dbus-run-session");
		return;
	}
	let lookups = Arc::new(AtomicU32::new(0));
	let portal = zbus::connection::Builder::session()
		.unwrap()
		.name("org.freedesktop.portal.Desktop")
		.unwrap()
		.serve_at("/org/freedesktop/portal/desktop", Resolver { lookups: lookups.clone() })
		.unwrap()
		.build()
		.await
		.unwrap();
	let proxy = proxy_for("https://capped.example:443/").await;
	assert_eq!(proxy.as_deref(), Some("socks5h://127.0.0.1:2080"));
	assert_eq!(proxy_for("https://other.example/").await, None);
	assert_eq!(lookups.load(Ordering::SeqCst), 2);
	// Kept: the same scheme, host and port are not asked again.
	let proxy = proxy_for("https://capped.example/a/b.png").await;
	assert_eq!(proxy.as_deref(), Some("socks5h://127.0.0.1:2080"));
	assert_eq!(lookups.load(Ordering::SeqCst), 2);
	// The portal leaves the bus: missing for a minute, not asked meanwhile.
	const PORTAL: &str = "org.freedesktop.portal.Desktop";
	assert!(portal.release_name(PORTAL).await.unwrap());
	assert_eq!(proxy_for("https://gone.example/").await, None);
	portal.request_name(PORTAL).await.unwrap();
	assert_eq!(proxy_for("https://capped.example:8443/").await, None);
	assert_eq!(lookups.load(Ordering::SeqCst), 2);
}
