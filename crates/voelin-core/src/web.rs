//! Pictures from the web: the host banner and TeamSpeak 6 channel banners,
//! which servers give as `http(s)` addresses on any host, myTeamSpeak
//! avatars, the pictures of the clients' badges on TeamSpeak's server
//! ([`voelin_model::badges::icon_url`]) and pictures in chat messages.
//!
//! They go into the engine's cache ([`crate::cache`], named by the MD5 hash
//! of the address) like avatars and icons, and only with
//! `cache.fetch_images`: fetching one contacts a host the server chose, as
//! the official client does. A picture is at most
//! [`cache::MAX_PICTURE_BYTES`] and goes to disk as it arrives. It is kept
//! only if its content is a picture the UI shows (PNG, JPEG, GIF, WebP,
//! SVG), whatever its address or the server say. Asked for again (a retry,
//! the host banner's reload), a cached picture is downloaded only if its
//! host says it changed (`If-None-Match`, `If-Modified-Since`; a `304`
//! answer has no body).
//!
//! Time limits: a connection (DNS, TCP and TLS) is made within
//! [`CONNECT_TIMEOUT`]. The answer must begin within [`READ_TIMEOUT`] of the
//! request (one deadline, not per read); then a download fails when nothing
//! arrives for [`READ_TIMEOUT`] or, after [`GRACE`], when it averages less
//! than [`cache::MIN_PICTURE_RATE`]: a large banner on a slow host still
//! arrives. A connection whose data stops (the network drops it, as some
//! ISPs do after the first 16 KB to some hosts) ends sooner on Linux and
//! Android: its keepalive probes (after [`KEEPALIVE`]) go unanswered, and
//! the kernel closes it about `TCP_USER_TIMEOUT` (30 s) after its last
//! data; on Windows and macOS [`READ_TIMEOUT`] ends it. The log calls a
//! stop in the data a stall, and says when the connection was HTTP/2's,
//! which carries a host's downloads together (what arrived is this
//! download's share). Before the answer began, on Windows and macOS, a
//! stall cannot be told from a slow host: "no answer within 60 s". A
//! connection never made is told apart.
//!
//! A failure tells when to try again ([`RetryHint`]): an address that is
//! wrong or gone (HTTP 400, 404, 410) not soon, a host that asks to wait
//! (HTTP 429, 503, `Retry-After`) after that wait.
//!
//! The client is the workspace's reqwest with rustls and the platform's
//! certificate store. Proxies: the one the desktop's settings name for an
//! address ([`set_proxy_resolver`]; GNOME's or KDE's through
//! xdg-desktop-portal on Linux), HTTP or SOCKS; when it names none, the
//! system's: the `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` variables, and on
//! Windows and macOS the system settings. Redirects are followed here, so
//! each hop goes through its own proxy; the log tells which proxy the
//! desktop names when that changes. At most [`PER_HOST`] downloads run per
//! host (a redirect's target counts), 16 in all: a host whose connections
//! stall does not hold them all.
//!
//! Banners in the server's own files (`ts3image://`) come through the voice
//! connection instead ([`crate::files::server_image`]).

use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use reqwest::header::{self, HeaderMap};
use reqwest::{StatusCode, Url, Version};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info};

use crate::cache::{self, Arrived, Cache, Fetch, FetchError, RetryHint, Validators, Waiter};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Quiet time before keepalive probes, and between them.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Linux, Android: how long what was sent (keepalive probes too) may stay
/// unanswered before the kernel ends the connection.
#[cfg(any(target_os = "linux", target_os = "android"))]
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a download may start slowly before its rate counts.
const GRACE: Duration = Duration::from_secs(120);
/// The first bytes, enough to tell a picture from an error page (SVG with
/// a comment or a DOCTYPE before it too).
const SNIFF: usize = 4096;
/// The pictures the UI decodes, so a host choosing between formats
/// (`Accept`) does not answer with one it cannot show (AVIF, JPEG XL).
const ACCEPT: &str = "image/png,image/jpeg,image/gif,image/webp,image/svg+xml,image/bmp,\
	image/x-icon;q=0.9,*/*;q=0.1";
/// Fetch independent URLs in parallel without letting a large channel tree
/// open unbounded connections or buffer unbounded image data.
static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);
/// Downloads from one host (and port) at once, as browsers allow.
const PER_HOST: usize = 6;
/// Redirects followed, as many as a browser does.
const REDIRECTS: usize = 20;
/// The host banner is reloaded at most this often, whatever the server
/// asks (TeamSpeak 3 and 6 servers refuse intervals below a minute).
pub(crate) const MIN_RELOAD: Duration = Duration::from_secs(60);
/// The shortest and the longest wait a busy host's `Retry-After` gets.
const RETRY_AFTER: (Duration, Duration) = (Duration::from_secs(60), Duration::from_secs(3600));
/// Characters of an address in the log ([`loggable`]).
const LOGGED_URL: usize = 300;

/// Names the proxy for an address: see [`set_proxy_resolver`].
type ProxyResolver =
	Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

/// Have pictures go through the proxy `resolve` names for each address
/// (given as `scheme://host:port/`): an HTTP or SOCKS proxy's URL as
/// reqwest takes it (`http://host:port`, `socks5h://host:port`), or `None`
/// for the system's (environment variables; on Windows and macOS the
/// system settings). On Linux: the desktop's settings,
/// `voelin_platform::proxy::proxy_for`. A later call replaces it.
pub fn set_proxy_resolver<F, R>(resolve: F)
where
	F: Fn(String) -> R + Send + Sync + 'static,
	R: Future<Output = Option<String>> + Send + 'static,
{
	CLIENTS.set_resolver(Arc::new(move |origin| Box::pin(resolve(origin))));
}

static CLIENTS: LazyLock<Clients> = LazyLock::new(|| Clients::new(READ_TIMEOUT));

/// The clients pictures are downloaded with: the system's, and one for
/// each proxy the resolver named.
struct Clients {
	read_timeout: Duration,
	resolver: RwLock<Option<ProxyResolver>>,
	direct: Result<reqwest::Client, String>,
	proxied: Mutex<HashMap<String, reqwest::Client>>,
	/// The proxy pictures from each origin went through last, for the log
	/// (without its user and password); `None`: the system's.
	routes: Mutex<HashMap<String, Option<String>>>,
}

impl Clients {
	fn new(read_timeout: Duration) -> Self {
		Self {
			read_timeout,
			resolver: RwLock::new(None),
			direct: build_client(read_timeout, None),
			proxied: Mutex::default(),
			routes: Mutex::default(),
		}
	}

	fn set_resolver(&self, resolver: ProxyResolver) {
		*self.resolver.write().unwrap_or_else(PoisonError::into_inner) = Some(resolver);
	}

	/// The client for `url`, and the proxy it goes through.
	async fn for_url(&self, url: &Url) -> Result<(reqwest::Client, Option<String>), String> {
		let resolver = self.resolver.read().unwrap_or_else(PoisonError::into_inner).clone();
		let origin = origin_of(url);
		let proxy = match resolver {
			Some(resolve) => resolve(origin.clone()).await,
			None => None,
		};
		let proxied = proxy.and_then(|proxy| self.proxied(proxy));
		self.note_route(&origin, proxied.as_ref().map(|(_, proxy)| proxy.as_str()));
		match proxied {
			Some((client, proxy)) => Ok((client, Some(proxy))),
			None => self.direct.clone().map(|client| (client, None)),
		}
	}

	/// The client through `proxy`, and `proxy`; `None` if reqwest refuses
	/// it.
	fn proxied(&self, proxy: String) -> Option<(reqwest::Client, String)> {
		let mut proxied = self.proxied.lock().unwrap_or_else(PoisonError::into_inner);
		if let Some(client) = proxied.get(&proxy) {
			return Some((client.clone(), proxy));
		}
		match build_client(self.read_timeout, Some(&proxy)) {
			Ok(client) => {
				proxied.insert(proxy.clone(), client.clone());
				Some((client, proxy))
			}
			Err(error) => {
				// Not one reqwest takes: as if there were none.
				let proxy = without_credentials(&proxy);
				debug!(%proxy, %error, "the desktop's proxy cannot be used for pictures");
				None
			}
		}
	}

	/// Log which proxy pictures from `origin` go through when that changes:
	/// once for each proxy the first time, then whenever an origin's
	/// changes (a desktop's proxy turned on or off). The system's at first
	/// is as without the desktop's settings: not told.
	fn note_route(&self, origin: &str, proxy: Option<&str>) {
		let proxy = proxy.map(without_credentials);
		let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
		let known = routes.values().any(|route| route.is_some() && *route == proxy);
		match (routes.insert(origin.to_owned(), proxy.clone()), proxy) {
			(Some(before), now) if before == now => {}
			(None, None) => {}
			(None, Some(proxy)) => {
				if !known {
					info!(%proxy, "pictures go through the desktop's proxy");
				}
			}
			(Some(_), Some(proxy)) => {
				info!(%origin, %proxy, "pictures from this host go through the desktop's proxy now");
			}
			(Some(_), None) => {
				info!(%origin, "pictures from this host go without the desktop's proxy now");
			}
		}
	}
}

/// A client, through `proxy` or the system's.
fn build_client(read_timeout: Duration, proxy: Option<&str>) -> Result<reqwest::Client, String> {
	let mut builder = reqwest::Client::builder()
		.connect_timeout(CONNECT_TIMEOUT)
		.read_timeout(read_timeout)
		.tcp_keepalive(KEEPALIVE)
		.tcp_keepalive_interval(KEEPALIVE)
		// Followed by `download`, each hop through its own proxy.
		.redirect(reqwest::redirect::Policy::none());
	#[cfg(any(target_os = "linux", target_os = "android"))]
	{
		builder = builder.tcp_user_timeout(TCP_USER_TIMEOUT);
	}
	if let Some(proxy) = proxy {
		builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| e.to_string())?);
	}
	builder.build().map_err(|e| e.to_string())
}

/// What a proxy is asked about: `url`'s scheme, host and port.
fn origin_of(url: &Url) -> String {
	let host = url.host_str().unwrap_or_default();
	match url.port_or_known_default() {
		Some(port) => format!("{}://{host}:{port}/", url.scheme()),
		None => format!("{}://{host}/", url.scheme()),
	}
}

/// A proxy's URL for the log: without its user and password.
fn without_credentials(proxy: &str) -> String {
	match Url::parse(proxy) {
		Ok(url) => {
			let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
			format!("{}://{}{port}", url.scheme(), url.host_str().unwrap_or("?"))
		}
		Err(_) => "?".to_owned(),
	}
}

/// What pictures are asked as: the app, and where to reach its makers (as
/// hosts such as Wikimedia ask).
fn user_agent() -> String {
	user_agent_of(&voelin_platform::crash::app_version())
}

/// [`user_agent`] of the app's version `app` (none: this crate's).
fn user_agent_of(app: &str) -> String {
	let version = if app.is_empty() { env!("CARGO_PKG_VERSION") } else { app };
	format!("Voelin/{version} (+https://github.com/Faumaray/Voelin)")
}

/// The hosts with downloads running or waiting, [`PER_HOST`] slots each.
static HOSTS: LazyLock<Mutex<HashMap<String, Arc<Semaphore>>>> = LazyLock::new(Mutex::default);

/// One of a host's [`PER_HOST`] download slots.
struct HostSlot {
	host: String,
	permit: Option<OwnedSemaphorePermit>,
}

impl HostSlot {
	async fn acquire(host: String) -> Self {
		let slots = HOSTS
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.entry(host.clone())
			.or_insert_with(|| Arc::new(Semaphore::new(PER_HOST)))
			.clone();
		let permit = slots.acquire_owned().await.expect("host slots stay open");
		Self { host, permit: Some(permit) }
	}
}

impl Drop for HostSlot {
	fn drop(&mut self) {
		let Some(permit) = self.permit.take() else { return };
		let slots = permit.semaphore().clone();
		drop(permit);
		let mut hosts = HOSTS.lock().unwrap_or_else(PoisonError::into_inner);
		// The map's and this one: no other download runs or waits.
		if Arc::strong_count(&slots) == 2
			&& hosts.get(&self.host).is_some_and(|s| Arc::ptr_eq(s, &slots))
		{
			hosts.remove(&self.host);
		}
	}
}

/// A download's turn to ask a host: one of its slots, then one of the 16
/// (the slot first: waiting for it holds none of the others).
struct Turn {
	_permit: tokio::sync::SemaphorePermit<'static>,
	slot: HostSlot,
}

impl Turn {
	async fn take(host: String) -> Self {
		let slot = HostSlot::acquire(host).await;
		let permit = DOWNLOADS.acquire().await.expect("download semaphore stays open");
		Self { _permit: permit, slot }
	}
}

/// Get the picture at `url` into `cache`, downloading it once however many
/// ask (again with `fresh`: it changes at its address, so its host is
/// asked whether it did); `waiter` is told where it is.
/// `max_cache_bytes`: `cache.max_mb` in bytes.
pub(crate) fn fetch(cache: Cache, url: &str, fresh: bool, max_cache_bytes: u64, waiter: Waiter) {
	fetch_within(cache, url, fresh, max_cache_bytes, LIMITS, waiter);
}

/// [`fetch`] a myTeamSpeak avatar: a picture of at most
/// [`AVATAR_LIMITS`]'s size, once (its link changes with it).
pub(crate) fn fetch_avatar(cache: Cache, url: &str, max_cache_bytes: u64, waiter: Waiter) {
	fetch_within(cache, url, false, max_cache_bytes, AVATAR_LIMITS, waiter);
}

/// [`fetch`] a picture a chat message shows: one of at most `max_bytes`
/// (up to [`cache::MAX_PICTURE_BYTES`]), once.
pub(crate) fn fetch_chat_picture(
	cache: Cache,
	url: &str,
	max_cache_bytes: u64,
	max_bytes: u64,
	waiter: Waiter,
) {
	let limits = Limits { max_bytes: max_bytes.min(LIMITS.max_bytes), ..LIMITS };
	fetch_within(cache, url, false, max_cache_bytes, limits, waiter);
}

fn fetch_within(
	cache: Cache,
	url: &str,
	fresh: bool,
	max_cache_bytes: u64,
	limits: Limits,
	waiter: Waiter,
) {
	let Some(key) = cache::picture_key(url) else {
		waiter(Err("not an http or https address".into()));
		return;
	};
	let Fetch::Download(temp) = cache.fetch(&key, fresh, waiter) else { return };
	// Asked for again: downloaded only if it changed.
	let validators = if fresh { cache.validators(&key) } else { None };
	let url = url.trim().to_owned();
	tokio::spawn(async move {
		let result = download(&url, &temp, &limits, validators.as_ref()).await;
		cache.finish_with(&key, &temp, result, max_cache_bytes);
	});
}

/// How large and how slow a download may be.
struct Limits {
	max_bytes: u64,
	grace: Duration,
	/// Bytes a second, on average since the start, once `grace` is over.
	min_rate: u64,
}

const LIMITS: Limits =
	Limits { max_bytes: cache::MAX_PICTURE_BYTES, grace: GRACE, min_rate: cache::MIN_PICTURE_RATE };
/// Avatars are small pictures (as myTeamSpeak's own, 4 MiB at most).
const AVATAR_LIMITS: Limits = Limits { max_bytes: 4 << 20, ..LIMITS };

/// Download the picture at `url` into `to` within `limits`, and only if it
/// is a picture (what arrived stays in `to` on failure; the cache removes
/// it). With the cached copy's `validators`, only if it changed. Each host
/// asked (a redirect's too) is asked in its [`Turn`].
async fn download(
	url: &str,
	to: &Path,
	limits: &Limits,
	validators: Option<&Validators>,
) -> Result<Arrived, FetchError> {
	download_with(&CLIENTS, url, to, limits, validators).await
}

async fn download_with(
	clients: &Clients,
	url: &str,
	to: &Path,
	limits: &Limits,
	validators: Option<&Validators>,
) -> Result<Arrived, FetchError> {
	let mut url = Url::parse(url)
		.map_err(|e| FetchError::new(format!("not a web address ({e})"), RetryHint::Permanent))?;
	let user_agent = user_agent();
	let mut hops = 0;
	let mut turn: Option<Turn> = None;
	let (mut response, proxy, sent) = loop {
		if !matches!(url.scheme(), "http" | "https") {
			let text = "redirected to an address not on the web";
			return Err(FetchError::new(text, RetryHint::Permanent));
		}
		let host = host_and_port(&url);
		if turn.as_ref().is_none_or(|turn| turn.slot.host != host) {
			// Another host: the last one's turn ends before waiting.
			drop(turn.take());
			turn = Some(Turn::take(host).await);
		}
		let (client, proxy) = clients.for_url(&url).await?;
		let mut request = client
			.get(url.clone())
			.header(header::ACCEPT, ACCEPT)
			.header(header::USER_AGENT, &user_agent);
		if let Some(validators) = validators {
			if let Some(etag) = &validators.etag {
				request = request.header(header::IF_NONE_MATCH, etag);
			}
			if let Some(date) = &validators.last_modified {
				request = request.header(header::IF_MODIFIED_SINCE, date);
			}
		}
		let sent = tokio::time::Instant::now();
		let response = match request.send().await {
			Ok(response) => response,
			Err(e) => {
				return Err(through(request_failed(e, clients.read_timeout), proxy.as_deref()));
			}
		};
		match redirect(&response, &url) {
			Some(_) if hops == REDIRECTS => return Err("too many redirects".into()),
			Some(next) => {
				hops += 1;
				url = next;
			}
			None => break (response, proxy, sent),
		}
	};
	let status = response.status();
	if status == StatusCode::NOT_MODIFIED && validators.is_some() {
		return Ok(Arrived::Unchanged);
	}
	if !status.is_success() {
		let error = status_error(status, response.headers(), SystemTime::now());
		return Err(through(error, proxy.as_deref()));
	}
	let too_big = || FetchError::from(format!("larger than {} KiB", limits.max_bytes >> 10));
	if response.content_length().is_some_and(|n| n > limits.max_bytes) {
		return Err(too_big());
	}
	let version = response.version();
	let arrived = Arrived::File(validators_of(response.headers()));
	if let Some(dir) = to.parent() {
		tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
	}
	let mut file = tokio::fs::File::create(to).await.map_err(|e| e.to_string())?;
	let started = tokio::time::Instant::now();
	let mut head = Vec::with_capacity(SNIFF);
	let mut received = 0u64;
	loop {
		let chunk = match response.chunk().await {
			Ok(Some(chunk)) => chunk,
			Ok(None) => break,
			Err(e) => {
				let failed = body_failed(e, received, version, sent.elapsed());
				return Err(through(failed, proxy.as_deref()));
			}
		};
		received += chunk.len() as u64;
		if received > limits.max_bytes {
			return Err(too_big());
		}
		if head.len() < SNIFF {
			head.extend_from_slice(&chunk[..chunk.len().min(SNIFF - head.len())]);
			// An error page is refused without waiting for all of it.
			if head.len() == SNIFF && !is_picture(&head) {
				return Err(not_a_picture(&head).into());
			}
		}
		let elapsed = started.elapsed();
		let expected = limits.min_rate.saturating_mul(elapsed.as_millis() as u64) / 1000;
		if elapsed > limits.grace && received < expected {
			return Err(
				format!("too slow: {} KiB in {} s", received >> 10, elapsed.as_secs()).into()
			);
		}
		file.write_all(&chunk).await.map_err(|e| e.to_string())?;
	}
	file.flush().await.map_err(|e| e.to_string())?;
	drop(file);
	if !is_picture(&head) {
		return Err(not_a_picture(&head).into());
	}
	Ok(arrived)
}

/// Where `response` redirects to (from `url`), if it is a redirect.
fn redirect(response: &reqwest::Response, url: &Url) -> Option<Url> {
	use StatusCode as S;
	let status = response.status();
	let redirects = [
		S::MOVED_PERMANENTLY,
		S::FOUND,
		S::SEE_OTHER,
		S::TEMPORARY_REDIRECT,
		S::PERMANENT_REDIRECT,
	];
	if !redirects.contains(&status) {
		return None;
	}
	let location = response.headers().get(header::LOCATION)?;
	url.join(std::str::from_utf8(location.as_bytes()).ok()?).ok()
}

/// What identifies the version of a picture, as its host sent it.
fn validators_of(headers: &HeaderMap) -> Validators {
	let value = |name| {
		headers
			.get(name)
			.and_then(|v| v.to_str().ok())
			.map(str::trim)
			.filter(|v| !v.is_empty() && v.len() <= 512)
			.map(str::to_owned)
	};
	Validators { etag: value(header::ETAG), last_modified: value(header::LAST_MODIFIED) }
}

/// A host's refusal, with when to ask again: not soon for an address that
/// is wrong or gone, when the host says for one too busy.
fn status_error(status: StatusCode, headers: &HeaderMap, now: SystemTime) -> FetchError {
	let retry = match status {
		StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::GONE => RetryHint::Permanent,
		StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
			match retry_after(headers, now) {
				Some(wait) => RetryHint::After(wait.clamp(RETRY_AFTER.0, RETRY_AFTER.1)),
				// Too many: not at once again, whatever it says.
				None if status == StatusCode::TOO_MANY_REQUESTS => RetryHint::After(RETRY_AFTER.0),
				None => RetryHint::Default,
			}
		}
		_ => RetryHint::Default,
	};
	FetchError::new(format!("HTTP {status}"), retry)
}

/// How long a host asks to wait (`Retry-After`: seconds, or a date).
fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
	let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
	if let Ok(seconds) = value.parse::<u64>() {
		return Some(Duration::from_secs(seconds));
	}
	let at = httpdate::parse_http_date(value).ok()?;
	Some(at.duration_since(now).unwrap_or_default())
}

/// `error`, saying the request went through `proxy`.
fn through(mut error: FetchError, proxy: Option<&str>) -> FetchError {
	if let Some(proxy) = proxy {
		error.text = format!("{} (through the proxy {})", error.text, without_credentials(proxy));
	}
	error
}

/// Why a request got no answer: a connection not made, one that stalled,
/// or another reason (TLS, DNS, …).
fn request_failed(error: reqwest::Error, read_timeout: Duration) -> FetchError {
	let error = error.without_url();
	if error.is_connect() {
		if error.is_timeout() {
			return format!("could not connect within {} s", CONNECT_TIMEOUT.as_secs()).into();
		}
		return describe(error).into();
	}
	match stall(&error) {
		// HTTP/2 carries a host's downloads on one connection: others on it
		// may have received data before it stopped.
		Some(Stall::Http2) => {
			return "the connection stalled before the answer began (HTTP/2; other downloads on \
				the same connection may have received data): the network stopped delivering data"
				.into();
		}
		Some(Stall::Own) => {
			return "the connection stalled before the answer began: the network stopped \
				delivering data"
				.into();
		}
		None => {}
	}
	if error.is_timeout() {
		// Where the kernel does not end a stalled connection (Windows,
		// macOS), it ends here too.
		let secs = read_timeout.as_secs();
		return format!(
			"no answer within {secs} s: the host is slow, or the network stopped delivering data"
		)
		.into();
	}
	describe(error).into()
}

/// Why a picture stopped arriving after `received` bytes, `elapsed` after
/// it was asked for (where a redirect led).
fn body_failed(
	error: reqwest::Error,
	received: u64,
	version: Version,
	elapsed: Duration,
) -> FetchError {
	let error = error.without_url();
	if stall(&error).is_none() && !error.is_timeout() {
		return describe(error).into();
	}
	let amount = if received < 1024 {
		format!("{received} bytes")
	} else {
		format!("{} KiB", received >> 10)
	};
	let version = match version {
		Version::HTTP_09 => "HTTP/0.9",
		Version::HTTP_10 => "HTTP/1.0",
		Version::HTTP_11 => "HTTP/1.1",
		// What arrived is this download's: the connection carries the
		// host's others too.
		Version::HTTP_2 => "HTTP/2; other downloads on the same connection may have received more",
		Version::HTTP_3 => "HTTP/3",
		_ => "HTTP",
	};
	let secs = elapsed.as_secs();
	format!(
		"the connection stalled after {amount} in {secs} s ({version}): the network stopped \
		 delivering data"
	)
	.into()
}

/// A connection that ended because its data stopped ([`stall`]).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stall {
	/// HTTP/2's, which carries other downloads from the host too.
	Http2,
	/// One this download had to itself.
	Own,
}

/// Whether `error` is a connection the kernel ended because its data
/// stopped: an I/O "timed out" among its causes. HTTP/2 keeps only the
/// kind of that error: in an `h2::Error` that names no cause, or rebuilt
/// from the kind (no OS error code), where a connection of its own keeps
/// the kernel's (`ETIMEDOUT`); with another h2 than hyper's, only its
/// text tells.
fn stall(error: &(dyn std::error::Error + 'static)) -> Option<Stall> {
	let mut last = error;
	let mut cause = Some(error);
	while let Some(error) = cause {
		if let Some(io) = error.downcast_ref::<h2::Error>().and_then(h2::Error::get_io)
			&& io.kind() == ErrorKind::TimedOut
		{
			return Some(Stall::Http2);
		}
		if let Some(io) = error.downcast_ref::<std::io::Error>()
			&& io.kind() == ErrorKind::TimedOut
		{
			return Some(if io.raw_os_error().is_some() { Stall::Own } else { Stall::Http2 });
		}
		last = error;
		cause = error.source();
	}
	(last.to_string() == "timed out").then_some(Stall::Http2)
}

/// What went wrong, for the log: the whole chain of causes (a TLS, DNS or
/// timeout reason); never the address (see [`loggable`]).
fn describe(error: reqwest::Error) -> String {
	let error = error.without_url();
	let mut text = if error.is_timeout() {
		"timed out".to_owned()
	} else if error.is_connect() {
		"could not connect".to_owned()
	} else {
		error.to_string()
	};
	let mut source = std::error::Error::source(&error);
	while let Some(cause) = source {
		text.push_str(": ");
		text.push_str(&cause.to_string());
		source = cause.source();
	}
	text
}

/// What arrived instead of a picture, for the log.
fn not_a_picture(head: &[u8]) -> String {
	let start = head.trim_ascii_start();
	let lower =
		start.get(..15.min(start.len())).map(<[u8]>::to_ascii_lowercase).unwrap_or_default();
	let what = if lower.starts_with(b"<!doctype html") || lower.starts_with(b"<html") {
		"an HTML page"
	} else if head.len() >= 12 && &head[4..8] == b"ftyp" {
		// ISO media, told by its brand.
		match &head[8..12] {
			b"avif" | b"avis" | b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx"
			| b"mif1" | b"msf1" => "AVIF or HEIF, which cannot be shown",
			_ => "a video (MP4, MOV), which cannot be shown",
		}
	} else if start.starts_with(b"{") {
		"JSON"
	} else {
		"something else"
	};
	format!("not a picture ({what})")
}

/// The host of `url`, for the log; the scheme for one without (a server
/// file, `ts3image://`).
pub(crate) fn host_of(url: &str) -> String {
	match reqwest::Url::parse(url.trim()) {
		Ok(u) if matches!(u.scheme(), "http" | "https") => u.host_str().unwrap_or("?").to_owned(),
		Ok(u) => u.scheme().to_owned(),
		Err(_) => "?".to_owned(),
	}
}

/// `url` for the log: `scheme://host[:port]/path`, without a user and
/// password, the query or the fragment (where signed links keep their
/// secrets), cut at [`LOGGED_URL`] characters; the scheme alone for an
/// address not on the web.
pub(crate) fn loggable(url: &str) -> String {
	let url = match reqwest::Url::parse(url.trim()) {
		Ok(u) if matches!(u.scheme(), "http" | "https") => u,
		Ok(u) => return u.scheme().to_owned(),
		Err(_) => return "?".to_owned(),
	};
	let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
	let mut text =
		format!("{}://{}{port}{}", url.scheme(), url.host_str().unwrap_or("?"), url.path());
	if let Some((at, _)) = text.char_indices().nth(LOGGED_URL) {
		text.truncate(at);
		text.push('…');
	}
	text
}

/// Which host's slots a download takes: `url`'s host and port.
fn host_and_port(url: &Url) -> String {
	format!("{}:{}", url.host_str().unwrap_or("?"), url.port_or_known_default().unwrap_or(0))
}

/// Whether `data` is a picture the UI decodes, told by its content as the
/// UI tells it (SVG by [`is_svg`]).
fn is_picture(data: &[u8]) -> bool {
	const STARTS: [&[u8]; 6] = [
		b"\x89PNG\r\n\x1a\n",
		b"\xff\xd8\xff",
		b"GIF87a",
		b"GIF89a",
		b"BM",
		// ICO.
		b"\0\0\x01\0",
	];
	STARTS.iter().any(|s| data.starts_with(s))
		|| (data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP")
		|| is_svg(data)
}

/// Whether `data` starts an SVG document: after a BOM and the prolog (the
/// XML declaration and other processing instructions, comments, blanks),
/// a DOCTYPE `svg` or an `svg` element. A page with an `svg` in it (an
/// HTML page, XHTML too) is not; nor is a start cut off before it tells.
pub fn is_svg(data: &[u8]) -> bool {
	let mut rest = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
	loop {
		rest = rest.trim_ascii_start();
		let (skipped, end) = if let Some(pi) = rest.strip_prefix(b"<?") {
			(pi, b"?>".as_slice())
		} else if let Some(comment) = rest.strip_prefix(b"<!--") {
			(comment, b"-->".as_slice())
		} else if rest.get(..9).is_some_and(|s| s.eq_ignore_ascii_case(b"<!doctype")) {
			// The root element it declares.
			let name = rest[9..].trim_ascii_start();
			return name.get(..3).is_some_and(|n| n.eq_ignore_ascii_case(b"svg"))
				&& name
					.get(3)
					.is_some_and(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'['));
		} else if let Some(element) = rest.strip_prefix(b"<") {
			return names_svg(element);
		} else {
			return false;
		};
		let Some(at) = skipped.windows(end.len()).position(|w| w == end) else {
			return false;
		};
		rest = &skipped[at + end.len()..];
	}
}

/// Whether the element named at the start of `tag` is `svg`, with a
/// namespace prefix or without.
fn names_svg(tag: &[u8]) -> bool {
	let Some(len) = tag.iter().position(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))
	else {
		return false;
	};
	let name = &tag[..len];
	name == b"svg" || name.ends_with(b":svg")
}

#[cfg(test)]
pub(crate) mod tests {
	use std::sync::mpsc;

	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	use tokio::net::{TcpListener, TcpStream};
	use tokio::sync::mpsc as async_mpsc;

	use super::*;

	const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\xdac\xf8\xcf\xc0\xf0\x1f\0\x05\0\x01\xff\x89\x99=\x1d\0\0\0\0IEND\xaeB`\x82";

	/// The heads of the requests a test server got.
	type Requests = Arc<Mutex<Vec<String>>>;

	/// An HTTP server on 127.0.0.1 with a few answers; returns its address.
	async fn server() -> String {
		logged_server().await.0
	}

	/// [`server`], and the requests it gets.
	async fn logged_server() -> (String, Requests) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let requests = Requests::default();
		let log = requests.clone();
		tokio::spawn(async move {
			loop {
				let Ok((mut socket, _)) = listener.accept().await else { return };
				let log = log.clone();
				tokio::spawn(async move {
					let mut request = Vec::new();
					let mut buf = [0u8; 1024];
					while !request.windows(4).any(|w| w == b"\r\n\r\n") {
						match socket.read(&mut buf).await {
							Ok(0) | Err(_) => return,
							Ok(n) => request.extend_from_slice(&buf[..n]),
						}
					}
					let request = String::from_utf8_lossy(&request).into_owned();
					log.lock().unwrap().push(request.clone());
					let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
					let mut extra = String::new();
					let (status, length, body): (&str, bool, Vec<u8>) = match path.as_str() {
						"/banner" => ("200 OK", true, PNG.to_vec()),
						"/svg" => {
							("200 OK", true, b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec())
						}
						// Above the earlier limits of 4 and 16 MiB.
						"/large-banner" => ("200 OK", true, [PNG, &vec![0; 20 << 20]].concat()),
						// Too big, as announced or as it comes.
						"/big" => ("200 OK", true, [PNG, &[0; 4000]].concat()),
						"/big-unannounced" => ("200 OK", false, [PNG, &[0; 4000]].concat()),
						"/page" => ("200 OK", true, b"<!DOCTYPE html><html></html>".to_vec()),
						"/long-page" | "/endless-page" => (
							"200 OK",
							false,
							[b"<!DOCTYPE html>".as_slice(), &vec![b' '; SNIFF]].concat(),
						),
						// A picture, then a byte every 100 ms.
						"/slow" => ("200 OK", true, [PNG, &[0; 20]].concat()),
						"/bad" => ("400 Bad Request", true, b"no".to_vec()),
						"/gone" => ("410 Gone", true, b"no".to_vec()),
						"/busy" => {
							extra = "Retry-After: 120\r\n".into();
							("429 Too Many Requests", true, b"later".to_vec())
						}
						"/busy-for-a-day" => {
							let date = httpdate::fmt_http_date(SystemTime::now() + 86400 * SECOND);
							extra = format!("Retry-After: {date}\r\n");
							("503 Service Unavailable", true, b"later".to_vec())
						}
						"/busy-a-moment" => {
							extra = "Retry-After: 1\r\n".into();
							("503 Service Unavailable", true, b"later".to_vec())
						}
						"/down" => ("503 Service Unavailable", true, b"later".to_vec()),
						// No answer at all.
						"/silent" => {
							tokio::time::sleep(Duration::from_secs(30)).await;
							return;
						}
						// Headers and 16 KiB of a megabyte, then nothing (the
						// network drops the rest).
						"/stall" => {
							("200 OK", false, [PNG, &vec![0; (16 << 10) - PNG.len()]].concat())
						}
						// Validators, and 304 to who has this version.
						"/versioned"
							if request.to_ascii_lowercase().contains("if-none-match: \"v1\"") =>
						{
							("304 Not Modified", false, Vec::new())
						}
						"/versioned" => {
							extra =
								"ETag: \"v1\"\r\nLast-Modified: Tue, 06 Oct 2026 10:00:00 GMT\r\n"
									.into();
							("200 OK", true, PNG.to_vec())
						}
						"/redirect" => {
							extra = "Location: /banner\r\n".into();
							("302 Found", true, Vec::new())
						}
						"/loop" => {
							extra = "Location: /loop\r\n".into();
							("301 Moved Permanently", true, Vec::new())
						}
						"/elsewhere" => {
							extra = "Location: file:///etc/hostname\r\n".into();
							("307 Temporary Redirect", true, Vec::new())
						}
						p if p.starts_with("/to?") => {
							extra = format!("Location: {}\r\n", &p[4..]);
							("302 Found", true, Vec::new())
						}
						_ => ("404 Not Found", true, b"no".to_vec()),
					};
					let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{extra}");
					if path == "/stall" {
						head += &format!("Content-Length: {}\r\n", 1 << 20);
					} else if length {
						head += &format!("Content-Length: {}\r\n", body.len());
					}
					head += "\r\n";
					let _ = socket.write_all(head.as_bytes()).await;
					if path == "/slow" {
						let (picture, rest) = body.split_at(PNG.len());
						let _ = socket.write_all(picture).await;
						for byte in rest {
							tokio::time::sleep(Duration::from_millis(100)).await;
							if socket.write_all(&[*byte]).await.is_err() {
								return;
							}
						}
						return;
					}
					let _ = socket.write_all(&body).await;
					// Never done: refused by its start or not at all.
					if path == "/endless-page" {
						while socket.write_all(b" ").await.is_ok() {
							tokio::time::sleep(Duration::from_millis(100)).await;
						}
					}
					if path == "/stall" {
						tokio::time::sleep(Duration::from_secs(30)).await;
					}
				});
			}
		});
		(format!("http://{addr}"), requests)
	}

	const SECOND: Duration = Duration::from_secs(1);

	/// What `f` logs at info (and above) on this thread: each line its
	/// message, then its fields (` name=value`).
	pub(crate) fn info_lines<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
		use tracing::field::{Field, Visit};
		use tracing::span;

		struct Lines(Arc<Mutex<Vec<String>>>);
		#[derive(Default)]
		struct Line(String, String);
		impl Visit for Line {
			fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
				match field.name() {
					"message" => self.0 = format!("{value:?}"),
					name => self.1 += &format!(" {name}={value:?}"),
				}
			}
		}
		impl tracing::Subscriber for Lines {
			fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
				*metadata.level() <= tracing::Level::INFO
			}
			fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
				span::Id::from_u64(1)
			}
			fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
			fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
			fn event(&self, event: &tracing::Event<'_>) {
				let mut line = Line::default();
				event.record(&mut line);
				self.0.lock().unwrap().push(line.0 + &line.1);
			}
			fn enter(&self, _: &span::Id) {}
			fn exit(&self, _: &span::Id) {}
		}
		let lines = Arc::new(Mutex::new(Vec::new()));
		let result = tracing::subscriber::with_default(Lines(lines.clone()), f);
		let lines = lines.lock().unwrap().clone();
		(result, lines)
	}

	fn temp_dir(tag: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("voelin-web-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	#[tokio::test]
	async fn downloads_pictures_only() {
		let base = server().await;
		let dir = temp_dir("download");
		let to = dir.join("tmp/x.part");
		let limits = Limits { max_bytes: 1000, ..LIMITS };
		let fresh = Arrived::File(Validators::default());
		assert_eq!(download(&format!("{base}/banner"), &to, &limits, None).await, Ok(fresh));
		assert_eq!(std::fs::read(&to).unwrap(), PNG);
		download(&format!("{base}/svg"), &to, &limits, None).await.unwrap();
		for (path, error) in [
			("/big", "larger than"),
			("/big-unannounced", "larger than"),
			("/page", "not a picture"),
			("/missing", "404"),
		] {
			let e = download(&format!("{base}{path}"), &to, &limits, None).await.unwrap_err();
			assert!(e.text.contains(error), "{path}: {e}");
		}
		// An error page longer than the first bytes looked at, unannounced:
		// refused by them, without waiting for the rest.
		for path in ["/long-page", "/endless-page"] {
			let url = format!("{base}{path}");
			let fetch = download(&url, &to, &LIMITS, None);
			let e = tokio::time::timeout(5 * SECOND, fetch).await.unwrap().unwrap_err();
			assert!(e.text.contains("not a picture (an HTML page)"), "{path}: {e}");
		}
		// Nothing listens there.
		let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
		let e = download(&format!("http://127.0.0.1:{port}/b"), &to, &limits, None).await;
		assert!(e.unwrap_err().text.starts_with("could not connect"));
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// An address wrong or gone is not tried again soon; a busy host is
	/// waited for as long as it asks, within reason.
	#[tokio::test]
	async fn failures_tell_when_to_try_again() {
		let base = server().await;
		let dir = temp_dir("hints");
		let to = dir.join("x");
		let minute = RetryHint::After(60 * SECOND);
		for (path, retry) in [
			("/missing", RetryHint::Permanent),
			("/bad", RetryHint::Permanent),
			("/gone", RetryHint::Permanent),
			("/busy", RetryHint::After(120 * SECOND)),
			("/busy-for-a-day", RetryHint::After(3600 * SECOND)),
			("/busy-a-moment", minute),
			("/down", RetryHint::Default),
			("/page", RetryHint::Default),
		] {
			let e = download(&format!("{base}{path}"), &to, &LIMITS, None).await.unwrap_err();
			assert_eq!(e.retry, retry, "{path}: {e}");
		}
		let e = download(&format!("{base}/missing"), &to, &LIMITS, None).await.unwrap_err();
		assert_eq!(e.text, "HTTP 404 Not Found");
		let mut headers = HeaderMap::new();
		let now = SystemTime::now();
		assert_eq!(retry_after(&headers, now), None);
		headers.insert(
			header::RETRY_AFTER,
			httpdate::fmt_http_date(now + 600 * SECOND).parse().unwrap(),
		);
		assert!(retry_after(&headers, now).is_some_and(|d| d.abs_diff(600 * SECOND) <= SECOND));
		headers.insert(
			header::RETRY_AFTER,
			httpdate::fmt_http_date(now - 600 * SECOND).parse().unwrap(),
		);
		assert_eq!(retry_after(&headers, now), Some(Duration::ZERO));
		headers.insert(header::RETRY_AFTER, "soon".parse().unwrap());
		assert_eq!(retry_after(&headers, now), None);
		// Without Retry-After: too many requests are not repeated at once.
		let e = status_error(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), now);
		assert_eq!(e.retry, minute);
		let _ = std::fs::remove_dir_all(dir);
	}

	/// The network stops delivering mid-picture: named a stall, with what
	/// arrived, not a bare "timed out". Before the answer began it may be a
	/// slow host too.
	#[tokio::test]
	async fn a_stalled_download_is_named() {
		let base = server().await;
		let dir = temp_dir("stall");
		let clients = Clients::new(SECOND);
		let (url, to) = (format!("{base}/stall"), dir.join("x"));
		let fetch = download_with(&clients, &url, &to, &LIMITS, None);
		let e = tokio::time::timeout(10 * SECOND, fetch).await.unwrap().unwrap_err();
		let rest = e.text.strip_prefix("the connection stalled after 16 KiB in ");
		let (secs, rest) = rest.and_then(|r| r.split_once(' ')).expect(&e.text);
		// The test's read timeout, and what a busy machine adds.
		assert!((1..=5).contains(&secs.parse::<u64>().unwrap()), "{e}");
		assert_eq!(rest, "s (HTTP/1.1): the network stopped delivering data");
		assert_eq!(e.retry, RetryHint::Default);
		let url = format!("{base}/silent");
		let fetch = download_with(&clients, &url, &to, &LIMITS, None);
		let e = tokio::time::timeout(10 * SECOND, fetch).await.unwrap().unwrap_err();
		assert_eq!(
			e.text,
			"no answer within 1 s: the host is slow, or the network stopped delivering data"
		);
		let _ = std::fs::remove_dir_all(dir);
	}

	/// A plain I/O error.
	#[derive(Debug)]
	struct Cause(&'static str, Option<Box<dyn std::error::Error + Send + Sync>>);

	impl std::fmt::Display for Cause {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str(self.0)
		}
	}

	impl std::error::Error for Cause {
		fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
			self.1.as_deref().map(|e| e as _)
		}
	}

	/// A stream that fails its reads with "timed out" once told, as a
	/// socket does when the kernel gives up on its connection.
	struct Stalling {
		inner: tokio::io::DuplexStream,
		stalled: Arc<std::sync::atomic::AtomicBool>,
	}

	impl tokio::io::AsyncRead for Stalling {
		fn poll_read(
			mut self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
			buf: &mut tokio::io::ReadBuf<'_>,
		) -> std::task::Poll<std::io::Result<()>> {
			if self.stalled.load(std::sync::atomic::Ordering::SeqCst) {
				return std::task::Poll::Ready(Err(ErrorKind::TimedOut.into()));
			}
			Pin::new(&mut self.inner).poll_read(cx, buf)
		}
	}

	impl tokio::io::AsyncWrite for Stalling {
		fn poll_write(
			mut self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
			buf: &[u8],
		) -> std::task::Poll<std::io::Result<usize>> {
			Pin::new(&mut self.inner).poll_write(cx, buf)
		}

		fn poll_flush(
			mut self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<std::io::Result<()>> {
			Pin::new(&mut self.inner).poll_flush(cx)
		}

		fn poll_shutdown(
			mut self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<std::io::Result<()>> {
			Pin::new(&mut self.inner).poll_shutdown(cx)
		}
	}

	/// HTTP/2 ended by the kernel's timeout mid-body: its `h2::Error` names
	/// no cause and keeps only the kind, yet it is a stall. So is any chain
	/// that ends in that text (another h2 version), and an I/O timeout.
	#[tokio::test]
	async fn stalls_are_told_by_their_causes() {
		use std::sync::atomic::{AtomicBool, Ordering};

		let (client_io, server_io) = tokio::io::duplex(1 << 16);
		let stalled = Arc::new(AtomicBool::new(false));
		let client_io = Stalling { inner: client_io, stalled: stalled.clone() };
		let (streams, mut stream) = async_mpsc::unbounded_channel();
		tokio::spawn(async move {
			let mut connection = h2::server::handshake(server_io).await.unwrap();
			while let Some(Ok((_, mut respond))) = connection.accept().await {
				let mut body = respond.send_response(http::Response::new(()), false).unwrap();
				body.send_data(bytes::Bytes::from_static(&[0; 100]), false).unwrap();
				let _ = streams.send(body);
			}
		});
		let (send, connection) = h2::client::handshake(client_io).await.unwrap();
		tokio::spawn(connection);
		let mut send = send.ready().await.unwrap();
		let request = http::Request::get("https://h.test/b.png").body(()).unwrap();
		let (response, _) = send.send_request(request, true).unwrap();
		let mut body = response.await.unwrap().into_body();
		assert_eq!(body.data().await.unwrap().unwrap().len(), 100);
		// The network stops: the next read fails.
		stalled.store(true, Ordering::SeqCst);
		let mut more = stream.recv().await.unwrap();
		more.send_data(bytes::Bytes::from_static(&[0; 100]), false).unwrap();
		let error =
			tokio::time::timeout(5 * SECOND, body.data()).await.unwrap().unwrap().unwrap_err();
		assert_eq!(error.to_string(), "timed out");
		assert!(std::error::Error::source(&error).is_none());
		let body = Cause("error reading a body from connection", Some(Box::new(error)));
		assert_eq!(stall(&body), Some(Stall::Http2));
		// Before the answer, hyper rebuilds HTTP/2's error from its kind.
		let rebuilt = std::io::Error::from(ErrorKind::TimedOut);
		let request = Cause("client error (SendRequest)", Some(Box::new(rebuilt)));
		assert_eq!(stall(&request), Some(Stall::Http2));
		// Another h2 than hyper's: told by the text.
		let text = Cause("body", Some(Box::new(Cause("timed out", None))));
		assert_eq!(stall(&text), Some(Stall::Http2));
		// A connection of its own keeps the kernel's error.
		let os = std::io::Error::from_raw_os_error(110);
		if os.kind() == ErrorKind::TimedOut {
			let request = Cause("client error (SendRequest)", Some(Box::new(os)));
			assert_eq!(stall(&request), Some(Stall::Own));
		}
		assert_eq!(stall(&Cause("connection reset", None)), None);
		assert_eq!(stall(&Cause("tls", Some(Box::new(Cause("operation timed out", None))))), None);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn fetch_into_the_cache() {
		let base = server().await;
		let dir = temp_dir("fetch");
		let cache = Cache::new(&dir);
		let (tx, rx) = mpsc::channel();
		let waiter = |tx: &mpsc::Sender<_>| -> Waiter {
			let tx = tx.clone();
			Box::new(move |r| tx.send(r).unwrap())
		};
		let url = format!("{base}/banner");
		fetch(cache.clone(), &url, false, 0, waiter(&tx));
		let path = tokio::task::spawn_blocking(move || rx.recv().unwrap()).await.unwrap().unwrap();
		assert_eq!(path, dir.join(cache::picture_key(&url).unwrap()));
		assert_eq!(std::fs::read(&path).unwrap(), PNG);
		// Not on the web: refused at once, nothing cached.
		let (tx, rx) = mpsc::channel();
		fetch(cache.clone(), "file:///etc/hostname", false, 0, waiter(&tx));
		assert!(rx.recv().unwrap().unwrap_err().text.contains("http"));
		// A failed download is reported and leaves nothing behind.
		fetch(cache.clone(), &format!("{base}/page"), false, 0, waiter(&tx));
		let failed = tokio::task::spawn_blocking(move || rx.recv().unwrap()).await.unwrap();
		assert_eq!(failed, Err("not a picture (an HTML page)".into()));
		// So does one larger than asked for (a chat picture).
		let (tx, rx) = mpsc::channel();
		fetch_chat_picture(cache.clone(), &format!("{base}/big"), 0, 1000, waiter(&tx));
		let failed = tokio::task::spawn_blocking(move || rx.recv().unwrap()).await.unwrap();
		assert!(failed.unwrap_err().text.contains("larger"));
		assert_eq!(cache.size(), PNG.len() as u64);
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Fetched again, a cached picture is asked for with its validators and
	/// kept as it is when its host says it did not change.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_cached_picture_is_downloaded_again_only_if_it_changed() {
		let (base, requests) = logged_server().await;
		let dir = temp_dir("revalidate");
		let cache = Cache::new(&dir);
		let url = format!("{base}/versioned");
		let key = cache::picture_key(&url).unwrap();
		let get = |fresh| {
			let (tx, rx) = mpsc::channel();
			fetch(cache.clone(), &url, fresh, 0, Box::new(move |r| tx.send(r).unwrap()));
			tokio::task::spawn_blocking(move || rx.recv().unwrap())
		};
		let path = get(false).await.unwrap().unwrap();
		assert_eq!(cache.validators(&key).unwrap().etag.as_deref(), Some("\"v1\""));
		std::fs::write(&path, PNG).unwrap();
		assert_eq!(get(true).await.unwrap(), Ok(path.clone()));
		assert_eq!(std::fs::read(&path).unwrap(), PNG);
		let requests = requests.lock().unwrap().clone();
		assert_eq!(requests.len(), 2);
		let first = requests[0].to_ascii_lowercase();
		let again = requests[1].to_ascii_lowercase();
		assert!(!first.contains("if-none-match"), "{first}");
		assert!(again.contains("if-none-match: \"v1\""), "{again}");
		assert!(again.contains("if-modified-since: tue, 06 oct 2026 10:00:00 gmt"), "{again}");
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Redirects are followed (each hop asked as the first), not forever,
	/// and only on the web; the app tells who it is and how to reach its
	/// makers.
	#[tokio::test]
	async fn redirects_and_who_asks() {
		let (base, requests) = logged_server().await;
		let dir = temp_dir("redirects");
		let to = dir.join("x");
		download(&format!("{base}/redirect"), &to, &LIMITS, None).await.unwrap();
		assert_eq!(std::fs::read(&to).unwrap(), PNG);
		let e = download(&format!("{base}/loop"), &to, &LIMITS, None).await.unwrap_err();
		assert_eq!(e.text, "too many redirects");
		let e = download(&format!("{base}/elsewhere"), &to, &LIMITS, None).await.unwrap_err();
		assert_eq!(e.retry, RetryHint::Permanent, "{e}");
		let requests = requests.lock().unwrap().clone();
		assert_eq!(requests.iter().filter(|r| r.starts_with("GET /loop ")).count(), REDIRECTS + 1);
		let user_agent = format!("user-agent: {}\r\n", user_agent().to_ascii_lowercase());
		for request in &requests {
			assert!(request.to_ascii_lowercase().contains(&user_agent), "{request}");
		}
		let _ = std::fs::remove_dir_all(dir);
	}

	/// A forward HTTP proxy on 127.0.0.1: relays each request, tells the
	/// request lines it got.
	async fn http_proxy() -> (u16, async_mpsc::UnboundedReceiver<String>) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		let (lines, received) = async_mpsc::unbounded_channel();
		tokio::spawn(async move {
			while let Ok((mut client, _)) = listener.accept().await {
				let lines = lines.clone();
				tokio::spawn(async move {
					let mut head = Vec::new();
					let mut byte = [0u8];
					while !head.ends_with(b"\r\n\r\n") {
						client.read_exact(&mut byte).await.unwrap();
						head.push(byte[0]);
					}
					let head = String::from_utf8(head).unwrap();
					let (line, rest) = head.split_once("\r\n").unwrap();
					lines.send(line.to_owned()).unwrap();
					// GET http://host:port/path HTTP/1.1, to the host as
					// GET /path HTTP/1.1.
					let target = line.split(' ').nth(1).unwrap();
					let url = Url::parse(target).unwrap();
					let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
					let mut server = TcpStream::connect(address).await.unwrap();
					let line = format!("GET {} HTTP/1.1\r\n", url.path());
					server.write_all(line.as_bytes()).await.unwrap();
					server.write_all(rest.as_bytes()).await.unwrap();
					let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
				});
			}
		});
		(port, received)
	}

	/// A SOCKS5 proxy on 127.0.0.1 without authentication: relays each
	/// connection, tells where to.
	async fn socks5_proxy() -> (u16, async_mpsc::UnboundedReceiver<String>) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		let (targets, received) = async_mpsc::unbounded_channel();
		tokio::spawn(async move {
			while let Ok((mut client, _)) = listener.accept().await {
				let targets = targets.clone();
				tokio::spawn(async move {
					let mut greeting = [0u8; 2];
					client.read_exact(&mut greeting).await.unwrap();
					let mut methods = vec![0u8; usize::from(greeting[1])];
					client.read_exact(&mut methods).await.unwrap();
					client.write_all(&[5, 0]).await.unwrap();
					let mut request = [0u8; 4];
					client.read_exact(&mut request).await.unwrap();
					let host = match request[3] {
						1 => {
							let mut ip = [0u8; 4];
							client.read_exact(&mut ip).await.unwrap();
							std::net::Ipv4Addr::from(ip).to_string()
						}
						3 => {
							let mut len = [0u8];
							client.read_exact(&mut len).await.unwrap();
							let mut name = vec![0u8; usize::from(len[0])];
							client.read_exact(&mut name).await.unwrap();
							String::from_utf8(name).unwrap()
						}
						other => panic!("address type {other}"),
					};
					let mut port = [0u8; 2];
					client.read_exact(&mut port).await.unwrap();
					let target = format!("{host}:{}", u16::from_be_bytes(port));
					targets.send(target.clone()).unwrap();
					let mut server = TcpStream::connect(target).await.unwrap();
					client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
					let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
				});
			}
		});
		(port, received)
	}

	/// The proxy the desktop names for each address: asked per hop, only
	/// with the scheme, host and port, and pictures go through it (HTTP or
	/// SOCKS); where it names none, they go as without it.
	#[tokio::test]
	async fn pictures_go_through_the_proxy_named_for_each_hop() {
		let base = server().await;
		let other = server().await;
		let (http_port, mut http_lines) = http_proxy().await;
		let (socks_port, mut socks_targets) = socks5_proxy().await;
		let clients = Clients::new(READ_TIMEOUT);
		let (asked, mut origins) = async_mpsc::unbounded_channel();
		let proxied = format!("{other}/");
		clients.set_resolver(Arc::new(move |origin: String| {
			asked.send(origin.clone()).unwrap();
			let answer =
				(origin == proxied).then(|| format!("http://user:secret@127.0.0.1:{http_port}"));
			Box::pin(async move { answer })
		}));
		let dir = temp_dir("proxy");
		let to = dir.join("x");
		// Direct to `base`, redirected to `other` through the HTTP proxy.
		let url = format!("{base}/to?{other}/banner");
		download_with(&clients, &url, &to, &LIMITS, None).await.unwrap();
		assert_eq!(std::fs::read(&to).unwrap(), PNG);
		assert_eq!(origins.recv().await.unwrap(), format!("{base}/"));
		assert_eq!(origins.recv().await.unwrap(), format!("{other}/"));
		assert_eq!(http_lines.recv().await.unwrap(), format!("GET {other}/banner HTTP/1.1"));
		assert!(http_lines.try_recv().is_err(), "only the second hop went through it");
		// Its failures say so, without the password.
		let e = download_with(&clients, &format!("{other}/gone"), &to, &LIMITS, None).await;
		let e = e.unwrap_err();
		assert!(
			e.text.ends_with(&format!("(through the proxy http://127.0.0.1:{http_port})")),
			"{e}"
		);
		assert!(!e.text.contains("secret"));
		// SOCKS, the host resolved by the proxy.
		let socks_clients = Clients::new(READ_TIMEOUT);
		let socks = format!("socks5h://127.0.0.1:{socks_port}");
		socks_clients.set_resolver(Arc::new(move |_| {
			let answer = socks.clone();
			Box::pin(async move { Some(answer) })
		}));
		let banner = format!("{}/banner", base.replace("127.0.0.1", "localhost"));
		download_with(&socks_clients, &banner, &to, &LIMITS, None).await.unwrap();
		assert_eq!(std::fs::read(&to).unwrap(), PNG);
		let port = base.rsplit(':').next().unwrap();
		assert_eq!(socks_targets.recv().await.unwrap(), format!("localhost:{port}"));
		let _ = std::fs::remove_dir_all(dir);
	}

	#[tokio::test]
	async fn banners_above_the_old_limits_are_downloaded() {
		let base = server().await;
		let dir = temp_dir("large-banner");
		let path = dir.join("large");
		download(&format!("{base}/large-banner"), &path, &LIMITS, None).await.unwrap();
		assert!(std::fs::metadata(&path).unwrap().len() > 16 << 20);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn a_trickling_download_ends_after_its_grace() {
		let base = server().await;
		let dir = temp_dir("slow");
		let to = dir.join("slow");
		// Slow, but within its grace: it arrives.
		let patient = Limits { grace: Duration::from_secs(60), ..LIMITS };
		download(&format!("{base}/slow"), &to, &patient, None).await.unwrap();
		let strict = Limits { grace: Duration::from_millis(150), min_rate: 1 << 20, ..LIMITS };
		let e = download(&format!("{base}/slow"), &to, &strict, None).await.unwrap_err();
		assert!(e.text.contains("too slow"), "{e}");
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A server on 127.0.0.1 that holds every answer until released; tells
	/// each request as it arrives.
	async fn gated_server() -> (String, async_mpsc::UnboundedReceiver<()>, Arc<Semaphore>) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		let (requests, received) = async_mpsc::unbounded_channel();
		let release = Arc::new(Semaphore::new(0));
		let gate = release.clone();
		tokio::spawn(async move {
			while let Ok((mut socket, _)) = listener.accept().await {
				let (requests, gate) = (requests.clone(), gate.clone());
				tokio::spawn(async move {
					let mut request = Vec::new();
					let mut buf = [0; 1024];
					while !request.windows(4).any(|w| w == b"\r\n\r\n") {
						let n = socket.read(&mut buf).await.unwrap();
						assert!(n > 0);
						request.extend_from_slice(&buf[..n]);
					}
					requests.send(()).unwrap();
					let permit = gate.acquire().await.unwrap();
					permit.forget();
					let head = format!(
						"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
						PNG.len()
					);
					socket.write_all(head.as_bytes()).await.unwrap();
					socket.write_all(PNG).await.unwrap();
				});
			}
		});
		(base, received, release)
	}

	#[tokio::test]
	async fn distinct_banners_download_concurrently_and_duplicates_share_a_request() {
		let (base, mut received, release) = gated_server().await;
		let dir = temp_dir("parallel");
		let cache = Cache::new(&dir);
		let (completed, mut results) = async_mpsc::unbounded_channel();
		for path in ["one", "two", "three", "one"] {
			let completed = completed.clone();
			fetch(
				cache.clone(),
				&format!("{base}/{path}"),
				false,
				0,
				Box::new(move |r| {
					completed.send(r).unwrap();
				}),
			);
		}
		// All three requests arrive before any response is released: no
		// slow banner can serialize the other downloads.
		for _ in 0..3 {
			tokio::time::timeout(5 * SECOND, received.recv()).await.unwrap().unwrap();
		}
		release.add_permits(3);
		for _ in 0..4 {
			let path =
				tokio::time::timeout(5 * SECOND, results.recv()).await.unwrap().unwrap().unwrap();
			assert_eq!(std::fs::read(path).unwrap(), PNG);
		}
		assert!(received.try_recv().is_err(), "duplicate URL opened another connection");
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A host whose answers do not come holds [`PER_HOST`] downloads at
	/// most, those redirected to it too; the others wait for a slot of
	/// their host.
	#[tokio::test]
	async fn a_host_runs_a_few_downloads_at_once() {
		let (base, mut received, release) = gated_server().await;
		// Redirected there from two hosts, fewer from each than a host's
		// slots.
		let redirectors = [server().await, server().await];
		let dir = temp_dir("per-host");
		let cache = Cache::new(&dir);
		let (completed, mut results) = async_mpsc::unbounded_channel();
		for n in 0..PER_HOST + 2 {
			let completed = completed.clone();
			let waiter: Waiter = Box::new(move |r| completed.send(r).unwrap());
			let url = format!("{}/to?{base}/{n}", redirectors[n % 2]);
			fetch(cache.clone(), &url, false, 0, waiter);
		}
		for _ in 0..PER_HOST {
			tokio::time::timeout(5 * SECOND, received.recv()).await.unwrap().unwrap();
		}
		let more = tokio::time::timeout(Duration::from_millis(300), received.recv()).await;
		assert!(more.is_err(), "more than {PER_HOST} downloads from one host at once");
		// Another host is not held up.
		let banner = format!("{}/banner", server().await);
		let (tx, rx) = mpsc::channel();
		fetch(cache.clone(), &banner, false, 0, Box::new(move |r| tx.send(r).unwrap()));
		tokio::task::spawn_blocking(move || rx.recv_timeout(5 * SECOND).unwrap().unwrap())
			.await
			.unwrap();
		release.add_permits(PER_HOST + 2);
		for _ in 0..PER_HOST + 2 {
			tokio::time::timeout(5 * SECOND, results.recv()).await.unwrap().unwrap().unwrap();
		}
		// The hosts' slots go with their last download.
		let hosts = [&base, &redirectors[0], &redirectors[1]]
			.map(|url| host_and_port(&Url::parse(url).unwrap()));
		let left = || hosts.iter().any(|host| HOSTS.lock().unwrap().contains_key(host));
		for _ in 0..100 {
			if !left() {
				break;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		assert!(!left(), "slots left behind");
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn addresses_for_the_log() {
		assert_eq!(
			loggable(
				" https://u:p@upload.wikimedia.org:443/wikipedia/commons/thumb/a/a9/Golod.jpg/250px-Golod.jpg?sig=s#f "
			),
			"https://upload.wikimedia.org/wikipedia/commons/thumb/a/a9/Golod.jpg/250px-Golod.jpg"
		);
		// Escaped as it is sent.
		assert_eq!(
			loggable("https://h.test/Голод 1.png"),
			"https://h.test/%D0%93%D0%BE%D0%BB%D0%BE%D0%B4%201.png"
		);
		assert_eq!(loggable("http://127.0.0.1:8080/b.png?x"), "http://127.0.0.1:8080/b.png");
		assert_eq!(loggable("http://[::1]:81/b"), "http://[::1]:81/b");
		let long = format!("https://h.test/{}", "a".repeat(1000));
		let logged = loggable(&long);
		assert_eq!(logged.chars().count(), LOGGED_URL + 1);
		assert!(logged.starts_with("https://h.test/aaa") && logged.ends_with("a…"));
		assert_eq!(loggable("ts3image://banner.png?channel=1"), "ts3image");
		assert_eq!(loggable("not an address"), "?");
		assert_eq!(without_credentials("socks5h://u:p@127.0.0.1:2080"), "socks5h://127.0.0.1:2080");
		assert_eq!(
			origin_of(&Url::parse("https://u:p@h.test/a?b").unwrap()),
			"https://h.test:443/"
		);
		assert_eq!(host_and_port(&Url::parse("http://H.test/x").unwrap()), "h.test:80");
		// The app's version, as the UI sets it.
		let link = "(+https://github.com/Faumaray/Voelin)";
		assert_eq!(user_agent_of("9.9.9-test"), format!("Voelin/9.9.9-test {link}"));
		assert_eq!(user_agent_of(""), format!("Voelin/{} {link}", env!("CARGO_PKG_VERSION")));
	}

	/// Which proxy pictures go through is told once for each, and when it
	/// changes for a host; never with its password.
	#[test]
	fn the_proxy_pictures_go_through_is_told_when_it_changes() {
		let clients = Clients::new(READ_TIMEOUT);
		let proxy = Some("http://u:secret@127.0.0.1:2080");
		let ((), lines) = info_lines(|| {
			clients.note_route("https://a.test:443/", None);
			clients.note_route("https://b.test:443/", proxy);
			clients.note_route("https://c.test:443/", proxy);
			clients.note_route("https://b.test:443/", proxy);
			clients.note_route("https://b.test:443/", None);
			clients.note_route("https://a.test:443/", Some("socks5h://127.0.0.1:1080"));
		});
		assert_eq!(
			lines,
			[
				"pictures go through the desktop's proxy proxy=http://127.0.0.1:2080",
				"pictures from this host go without the desktop's proxy now origin=https://b.test:443/",
				"pictures from this host go through the desktop's proxy now origin=https://a.test:443/ \
				 proxy=socks5h://127.0.0.1:1080",
			]
		);
	}

	#[test]
	fn pictures_by_content() {
		assert!(is_picture(PNG));
		assert!(is_picture(b"\xff\xd8\xff\xe0\0\x10JFIF"));
		assert!(is_picture(b"GIF89a\x01\0\x01\0"));
		assert!(is_picture(b"RIFF\x24\0\0\0WEBPVP8 "));
		assert!(is_picture(b"<?xml version=\"1.0\"?><svg/>"));
		assert!(is_picture(b"\xef\xbb\xbf\n <svg/>"));
		assert!(is_picture(b" \r\n<svg/>"));
		assert!(!is_picture(b"<!DOCTYPE html>"));
		assert!(!is_picture(b"RIFF\x24\0\0\0WAVEfmt "));
		assert!(!is_picture(b""));
		// What browsers show too: BMP, ICO, SVG after a comment or DOCTYPE.
		assert!(is_picture(b"BM\x36\0\0\0"));
		assert!(is_picture(b"\0\0\x01\0\x01\0"));
		assert!(is_picture(b"<!-- made with a tool -->\n<svg/>"));
		assert!(is_picture(b"<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\"><svg/>"));
		assert!(!is_picture(b"<!-- a page --><html></html>"));
		assert!(is_picture(b"<svg:svg xmlns:svg='http://www.w3.org/2000/svg'/>"));
		assert!(is_picture(
			b"<?xml version=\"1.0\"?><!-- c --><!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"x\"><svg/>"
		));
		assert!(is_picture(b"<!DOCTYPE svg [ <!ENTITY e \"x\"> ]><svg/>"));
		// Pages with an SVG in them are pages.
		assert!(!is_picture(
			b"<!DOCTYPE html><html><head><link rel=icon href=\"data:image/svg+xml,<svg/>\"></head></html>"
		));
		assert!(!is_picture(b"<!-- x --><html><body><svg/></body></html>"));
		assert!(!is_picture(b"<?xml version=\"1.0\"?><!DOCTYPE html><html><svg/></html>"));
		assert!(!is_picture(b"<svgx/>"));
		// Cut off before it tells.
		assert!(!is_picture(b"<?xml version=\"1.0\"?><!-- a long comment"));
		assert!(!is_picture(b"<svg"));
		// A cursor (CUR) is not shown.
		assert!(!is_picture(b"\0\0\x02\0\x01\0"));
		// What came instead is told.
		assert_eq!(not_a_picture(b"<!DOCTYPE html><html>"), "not a picture (an HTML page)");
		assert_eq!(
			not_a_picture(b"\0\0\0\x1cftypavif"),
			"not a picture (AVIF or HEIF, which cannot be shown)"
		);
		assert_eq!(
			not_a_picture(b"\0\0\0\x20ftypisom\0\0\x02\0isomiso2avc1mp41"),
			"not a picture (a video (MP4, MOV), which cannot be shown)"
		);
		assert_eq!(host_of(" https://cdn.example.test/b.png?sig=x "), "cdn.example.test");
		assert_eq!(host_of("ts3image://banner.png?channel=1"), "ts3image");
	}
}
