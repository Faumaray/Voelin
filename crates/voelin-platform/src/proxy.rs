//! The proxy the desktop's network settings name for an address.
//!
//! Linux asks xdg-desktop-portal's ProxyResolver (feature `portal`), which
//! answers from GNOME's or KDE's settings (a PAC script too), as browsers
//! follow them. It is given a second; answers are kept a minute per
//! scheme, host and port, no answer too (no portal, too slow, an error), so
//! a desktop without the portal costs one try a minute. An answer that
//! comes late (a portal the first call starts) replaces the "none" kept
//! meanwhile. Other platforms answer none: there the HTTP client reads the
//! system's settings itself.

/// How long the desktop may take to answer.
#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
/// How long an answer is kept.
#[cfg(any(test, all(unix, not(target_os = "macos"), feature = "portal")))]
const KEEP: std::time::Duration = std::time::Duration::from_secs(60);

/// The proxy for `url` as an HTTP client takes it (`http://host:port`,
/// `socks5h://host:port`, with the user and password the settings have),
/// or `None`: none (`direct://`), or no answer. Call it within a Tokio
/// runtime.
pub async fn proxy_for(url: &str) -> Option<String> {
	#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
	return portal::proxy_for(url).await;
	#[cfg(not(all(unix, not(target_os = "macos"), feature = "portal")))]
	{
		let _ = url;
		None
	}
}

/// The proxy to use of a resolver's answer (GLib's form, in the order to
/// try): the first an HTTP client can use, `None` when that is
/// `direct://`. SOCKS names the host to the proxy (`socks5h`,
/// `socks4a`), as browsers do: the proxy resolves it.
pub fn usable_proxy<S: AsRef<str>>(answer: &[S]) -> Option<String> {
	for uri in answer {
		let uri = uri.as_ref().trim();
		let Some((scheme, rest)) = uri.split_once("://") else { continue };
		let scheme = match scheme.to_ascii_lowercase().as_str() {
			"direct" => return None,
			"http" => "http",
			"https" => "https",
			"socks" | "socks5" | "socks5h" => "socks5h",
			"socks4" | "socks4a" => "socks4a",
			// rtsp, ftp, …: not for HTTP.
			_ => continue,
		};
		let rest = rest.trim_end_matches('/');
		if !rest.is_empty() {
			return Some(format!("{scheme}://{rest}"));
		}
	}
	None
}

/// A resolver's answer for the log: without the user and password GLib
/// puts in a proxy's address (`http://user:password@host:port`).
#[cfg(any(test, all(unix, not(target_os = "macos"), feature = "portal")))]
fn for_log<S: AsRef<str>>(answer: &[S]) -> Vec<String> {
	let without_userinfo = |uri: &str| match uri.split_once("://") {
		Some((scheme, rest)) => {
			format!("{scheme}://{}", rest.rsplit_once('@').map_or(rest, |(_, host)| host))
		}
		None => uri.rsplit_once('@').map_or(uri, |(_, host)| host).to_owned(),
	};
	answer.iter().map(|uri| without_userinfo(uri.as_ref())).collect()
}

/// What answers are kept by: `scheme://host:port` of `url`, lowercase, the
/// port told when it is the scheme's own; `None` for no host.
#[cfg(any(test, all(unix, not(target_os = "macos"), feature = "portal")))]
fn origin(url: &str) -> Option<String> {
	let (scheme, rest) = url.trim().split_once("://")?;
	let scheme = scheme.to_ascii_lowercase();
	let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
	// Without the user and password.
	let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h).to_ascii_lowercase();
	if host_port.is_empty() {
		return None;
	}
	let has_port = match host_port.rfind(':') {
		// An IPv6 address without a port ends in `]`.
		Some(at) => !host_port[at..].contains(']'),
		None => false,
	};
	if has_port {
		return Some(format!("{scheme}://{host_port}"));
	}
	let port = match scheme.as_str() {
		"http" | "ws" => 80,
		"https" | "wss" => 443,
		_ => 0,
	};
	Some(format!("{scheme}://{host_port}:{port}"))
}

/// Answers kept for [`KEEP`].
#[cfg(any(test, all(unix, not(target_os = "macos"), feature = "portal")))]
#[derive(Default)]
struct Answers(std::collections::HashMap<String, (std::time::Instant, Option<String>)>);

#[cfg(any(test, all(unix, not(target_os = "macos"), feature = "portal")))]
impl Answers {
	/// The answer for `origin` if it is still fresh at `now`.
	fn get(&self, origin: &str, now: std::time::Instant) -> Option<Option<String>> {
		let (at, answer) = self.0.get(origin)?;
		(now.saturating_duration_since(*at) < KEEP).then(|| answer.clone())
	}

	/// Keep `answer` for `origin`, as it was at `now`, unless a newer one
	/// is kept (the stale ones go).
	fn put(&mut self, origin: String, now: std::time::Instant, answer: Option<String>) {
		self.0.retain(|_, (at, _)| now.saturating_duration_since(*at) < KEEP);
		if self.0.get(&origin).is_some_and(|(at, _)| *at > now) {
			return;
		}
		self.0.insert(origin, (now, answer));
	}
}

#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
mod portal {
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{LazyLock, Mutex, PoisonError};
	use std::time::Instant;

	use ashpd::desktop::proxy_resolver::ProxyResolver;
	use tracing::{debug, info};

	use super::{Answers, KEEP, TIMEOUT, for_log, origin, usable_proxy};

	/// How long a lookup no longer waited for may still take: its answer is
	/// kept when it comes (a portal the first call starts often takes
	/// longer than [`TIMEOUT`]).
	const LATE: std::time::Duration = std::time::Duration::from_secs(25);

	static RESOLVER: tokio::sync::OnceCell<ProxyResolver> = tokio::sync::OnceCell::const_new();
	static ANSWERS: LazyLock<Mutex<Answers>> = LazyLock::new(Mutex::default);
	/// Until when the portal is taken as missing (it could not be reached).
	static MISSING_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
	/// Whether a missing portal was logged (once a run).
	static MISSING_LOGGED: AtomicBool = AtomicBool::new(false);

	fn answers() -> std::sync::MutexGuard<'static, Answers> {
		ANSWERS.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub(super) async fn proxy_for(url: &str) -> Option<String> {
		let key = origin(url)?;
		let now = Instant::now();
		if let Some(answer) = answers().get(&key, now) {
			return answer;
		}
		let missing = *MISSING_UNTIL.lock().unwrap_or_else(PoisonError::into_inner);
		if missing.is_some_and(|until| now < until) {
			answers().put(key, now, None);
			return None;
		}
		let asked = tokio::spawn(ask(url.to_owned(), key.clone()));
		match tokio::time::timeout(TIMEOUT, asked).await {
			Ok(answer) => answer.unwrap_or_default(),
			Err(_) => {
				debug!(origin = %key, "the desktop's proxy resolver did not answer within a second");
				// None until its answer comes.
				answers().put(key, now, None);
				None
			}
		}
	}

	/// Ask the portal for `url`'s proxy and keep the answer as `key`'s,
	/// late too (within [`LATE`]).
	async fn ask(url: String, key: String) -> Option<String> {
		let answer = match tokio::time::timeout(LATE, lookup(&url)).await {
			Ok(Ok(answer)) => {
				debug!(origin = %key, answer = ?for_log(&answer), "the desktop's proxy");
				usable_proxy(&answer)
			}
			Ok(Err(Unreachable(e))) => {
				*MISSING_UNTIL.lock().unwrap_or_else(PoisonError::into_inner) =
					Some(Instant::now() + KEEP);
				if !MISSING_LOGGED.swap(true, Ordering::Relaxed) {
					info!(error = %e, "no desktop proxy settings (xdg-desktop-portal's ProxyResolver)");
				}
				None
			}
			Ok(Err(Failed(e))) => {
				debug!(origin = %key, error = %e, "the desktop's proxy resolver failed");
				None
			}
			Err(_) => {
				debug!(origin = %key, "the desktop's proxy resolver did not answer");
				None
			}
		};
		answers().put(key, Instant::now(), answer.clone());
		answer
	}

	/// Why there is no answer.
	enum Error {
		/// The portal cannot be reached (no session bus, no portal, no
		/// ProxyResolver in it).
		Unreachable(ashpd::Error),
		/// It failed for this address.
		Failed(ashpd::Error),
	}
	use Error::{Failed, Unreachable};

	async fn lookup(url: &str) -> Result<Vec<String>, Error> {
		let resolver = RESOLVER.get_or_try_init(ProxyResolver::new).await.map_err(Unreachable)?;
		let uri = ashpd::Uri::parse(url).map_err(|e| Failed(e.into()))?;
		// A portal not on the bus shows only now: making the resolver
		// does not ask it anything it must answer.
		let answer = resolver
			.lookup(&uri)
			.await
			.map_err(|e| if not_there(&e) { Unreachable(e) } else { Failed(e) })?;
		Ok(answer.iter().map(|uri| uri.as_str().to_owned()).collect())
	}

	/// Whether `error` says no portal answers on the bus (none running nor
	/// startable, or one without ProxyResolver), not that a lookup failed.
	pub(super) fn not_there(error: &ashpd::Error) -> bool {
		use ashpd::zbus::{self, fdo};

		let bus = match error {
			ashpd::Error::PortalNotFound(_) => return true,
			ashpd::Error::Zbus(e) | ashpd::Error::Portal(ashpd::PortalError::ZBus(e)) => e,
			_ => return false,
		};
		match bus {
			zbus::Error::MethodError(name, ..) => matches!(
				name.as_str(),
				"org.freedesktop.DBus.Error.ServiceUnknown"
					| "org.freedesktop.DBus.Error.NameHasNoOwner"
					| "org.freedesktop.DBus.Error.UnknownObject"
					| "org.freedesktop.DBus.Error.UnknownInterface"
					| "org.freedesktop.DBus.Error.UnknownMethod"
			),
			zbus::Error::FDO(e) => matches!(
				**e,
				fdo::Error::ServiceUnknown(_)
					| fdo::Error::NameHasNoOwner(_)
					| fdo::Error::UnknownObject(_)
					| fdo::Error::UnknownInterface(_)
					| fdo::Error::UnknownMethod(_)
			),
			_ => false,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use super::*;

	#[test]
	fn proxies_of_answers() {
		assert_eq!(usable_proxy(&["direct://"]), None);
		assert_eq!(usable_proxy::<&str>(&[]), None);
		assert_eq!(
			usable_proxy(&["http://127.0.0.1:2080", "direct://"]).as_deref(),
			Some("http://127.0.0.1:2080")
		);
		assert_eq!(
			usable_proxy(&["https://proxy.example:443/"]).as_deref(),
			Some("https://proxy.example:443")
		);
		// SOCKS: the proxy resolves the host.
		assert_eq!(
			usable_proxy(&["socks://127.0.0.1:2080"]).as_deref(),
			Some("socks5h://127.0.0.1:2080")
		);
		assert_eq!(usable_proxy(&["SOCKS5://u:p@h:1"]).as_deref(), Some("socks5h://u:p@h:1"));
		assert_eq!(usable_proxy(&["socks4://h:1080"]).as_deref(), Some("socks4a://h:1080"));
		assert_eq!(usable_proxy(&["socks5h://h:1080"]).as_deref(), Some("socks5h://h:1080"));
		// What HTTP cannot use is skipped; direct ends the list.
		assert_eq!(
			usable_proxy(&["rtsp://h:554", "http://h:3128"]).as_deref(),
			Some("http://h:3128")
		);
		assert_eq!(usable_proxy(&["direct://", "http://h:3128"]), None);
		assert_eq!(usable_proxy(&["nonsense", "http://"]), None);
	}

	/// The answer in the log has no passwords, which GLib puts in it.
	#[test]
	fn answers_are_logged_without_passwords() {
		let answer = ["http://u:secret@h:1", "socks://u:p/w@rd#x@127.0.0.1:2080", "direct://"];
		let logged = format!("{:?}", for_log(&answer));
		assert_eq!(logged, r#"["http://h:1", "socks://127.0.0.1:2080", "direct://"]"#);
		assert_eq!(for_log(&["u:secret@h:1"]), ["h:1"]);
	}

	#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
	#[test]
	fn a_portal_not_on_the_bus_is_missing() {
		use ashpd::zbus::{self, fdo};

		let fdo = |e: fdo::Error| zbus::Error::FDO(Box::new(e));
		let not_there = |e: &ashpd::Error| portal::not_there(e);
		let unknown = fdo(fdo::Error::ServiceUnknown("not activatable".into()));
		assert!(not_there(&ashpd::Error::Portal(ashpd::PortalError::ZBus(unknown))));
		let no_owner = fdo(fdo::Error::NameHasNoOwner("gone".into()));
		assert!(not_there(&ashpd::Error::Zbus(no_owner)));
		assert!(!not_there(&ashpd::Error::Zbus(fdo(fdo::Error::Failed("PAC".into())))));
		assert!(!not_there(&ashpd::Error::Portal(ashpd::PortalError::Failed("PAC".into()))));
	}

	#[test]
	fn answers_are_kept_by_scheme_host_and_port() {
		assert_eq!(
			origin("https://Upload.Wikimedia.org/a/b.png?x#y").as_deref(),
			Some("https://upload.wikimedia.org:443")
		);
		assert_eq!(origin("http://u:p@h.test:8080/").as_deref(), Some("http://h.test:8080"));
		assert_eq!(origin("http://[::1]/x").as_deref(), Some("http://[::1]:80"));
		assert_eq!(origin("http://[::1]:81").as_deref(), Some("http://[::1]:81"));
		assert_eq!(origin("https:///x"), None);
		assert_eq!(origin("no scheme"), None);
		let mut answers = Answers::default();
		let now = Instant::now();
		assert_eq!(answers.get("https://h:443", now), None);
		answers.put("https://h:443".into(), now, Some("http://p:1".into()));
		// No answer is kept too.
		answers.put("https://n:443".into(), now, None);
		let soon = now + Duration::from_secs(59);
		assert_eq!(answers.get("https://h:443", soon), Some(Some("http://p:1".into())));
		assert_eq!(answers.get("https://n:443", soon), Some(None));
		assert_eq!(answers.get("http://h:80", soon), None);
		let later = now + KEEP;
		assert_eq!(answers.get("https://h:443", later), None);
		answers.put("https://other:443".into(), later, None);
		assert_eq!(answers.0.len(), 1, "stale answers go");
		// A late answer stays: the "none" kept while it was awaited is older.
		let mut answers = Answers::default();
		answers.put(
			"https://h:443".into(),
			now + Duration::from_secs(3),
			Some("http://p:1".into()),
		);
		answers.put("https://h:443".into(), now, None);
		assert_eq!(answers.get("https://h:443", soon), Some(Some("http://p:1".into())));
	}

	/// Without a portal (or off Linux): no proxy, at once.
	#[tokio::test]
	async fn no_answer_is_no_proxy() {
		let started = Instant::now();
		let _ = proxy_for("https://example.com/").await;
		assert!(started.elapsed() < Duration::from_secs(5));
		assert_eq!(proxy_for("not an address").await, None);
	}
}
