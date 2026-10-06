//! Finding a server's `tsgw` gateway from the server's address, so nobody
//! types the gateway's URL.
//!
//! Like TeamSpeak's own lookup (SRV `_ts3._udp`, then TSDNS), at the
//! address's host and then each parent domain down to the last two labels,
//! the most specific name first:
//!
//! - SRV `_tsgws._tcp.<name>`: `wss://<target>:<port><path>`,
//! - SRV `_tsgw._tcp.<name>`: `ws://<target>:<port><path>`,
//!
//! `<path>` is `path=…` from a TXT record at the SRV record's name, `/v1`
//! without one. As TSDNS has its well-known port, the gateway itself is also
//! asked at tsgw's default port: `http://<name>:7788/.well-known/tsgw`
//! answers `{"url": "…"}`; for an IP address (or `localhost`) only that is
//! tried. That answer is plain HTTP, so it is used only when nothing or a
//! plain record is published: a TLS-only publication stays TLS-only.
//!
//! Everything is asked at once (SRV and TXT at every name, and the gateway
//! at every name), so a discovery takes as long as its slowest needed
//! answer, not their sum; DNS gives up after 2 s (twice).
//!
//! The most specific name with a record wins: a parent domain's records
//! are another server's gateway when the host publishes its own. Every
//! gateway of that name is kept, best first (TLS before plain, by
//! priority, then the gateway's own answer at that name), and tried in turn
//! ([`crate::Command::ObserveGateway`]): a TLS proxy that fails falls back
//! to the plain gateway published next to it. Discovery trusts DNS and the
//! network as TSDNS does; falling back from a published `_tsgws` to a
//! published `_tsgw` is no weaker than publishing only `_tsgw`. What an
//! admin publishes is in `docs/gateway-admin.md`. Each discovery is logged
//! (target `voelin_core::discover`, the details at debug).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, FutureExt, join_all};
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{CLOUDFLARE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RData;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, info};

/// The port tsgw listens on by default, where it is asked for its URL.
pub const DEFAULT_PORT: u16 = 7788;
/// Where tsgw serves its WebSocket.
const DEFAULT_PATH: &str = "/v1";
/// Give up on asking one name's gateway after this long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Give up on one DNS query after this long (it is sent twice).
const DNS_TIMEOUT: Duration = Duration::from_secs(2);
/// At most this many gateways are kept, best first.
const MAX_CANDIDATES: usize = 8;

type SrvFn = dyn Fn(String) -> BoxFuture<'static, Vec<(u16, String, u16)>> + Send + Sync;
type TxtFn = dyn Fn(String) -> BoxFuture<'static, Vec<String>> + Send + Sync;
type ProbeFn = dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync;

/// Where [`gateway`] gets its answers from; the tests put fakes in.
#[derive(Clone)]
struct Lookups {
	/// The SRV records at a name: priority, target, port.
	srv: Arc<SrvFn>,
	/// The strings of the TXT records at a name.
	txt: Arc<TxtFn>,
	/// The URL a gateway at a name gives for itself (`/.well-known/tsgw`).
	probe: Arc<ProbeFn>,
}

impl Lookups {
	fn system() -> Self {
		// The system's resolver, else (Android) a public one, as the voice
		// connection's resolver does. A query that is not answered is sent
		// again soon: discovery runs once a run, and finding nothing is
		// final until the next.
		let resolver = TokioResolver::builder_tokio()
			.map(|mut b| {
				let options = b.options_mut();
				options.timeout = DNS_TIMEOUT;
				options.attempts = options.attempts.max(2);
				b
			})
			.and_then(|b| b.build())
			.or_else(|_| {
				let mut b = TokioResolver::builder_with_config(
					ResolverConfig::udp_and_tcp(&CLOUDFLARE),
					TokioRuntimeProvider::default(),
				);
				b.options_mut().timeout = DNS_TIMEOUT;
				b.build()
			});
		let resolver = match resolver {
			Ok(resolver) => Some(resolver),
			Err(e) => {
				debug!(error = %e, "no DNS resolver for gateway discovery");
				None
			}
		};
		let txt_resolver = resolver.clone();
		Self {
			srv: Arc::new(move |name| {
				let resolver = resolver.clone();
				async move {
					let started = Instant::now();
					let lookup = resolver?.srv_lookup(name.as_str()).await;
					let lookup = dns_answer(&name, "SRV", started, lookup)?;
					Some(
						lookup
							.answers()
							.iter()
							.filter_map(|r| match &r.data {
								RData::SRV(s) => Some((s.priority, s.target.to_ascii(), s.port)),
								_ => None,
							})
							.collect(),
					)
				}
				.map(Option::unwrap_or_default)
				.boxed()
			}),
			txt: Arc::new(move |name| {
				let resolver = txt_resolver.clone();
				async move {
					let started = Instant::now();
					let lookup = resolver?.txt_lookup(name.as_str()).await;
					let lookup = dns_answer(&name, "TXT", started, lookup)?;
					Some(
						lookup
							.answers()
							.iter()
							.filter_map(|r| match &r.data {
								RData::TXT(t) => Some(t.txt_data.iter()),
								_ => None,
							})
							.flatten()
							.map(|s| String::from_utf8_lossy(s).into_owned())
							.collect(),
					)
				}
				.map(Option::unwrap_or_default)
				.boxed()
			}),
			probe: Arc::new(|name| {
				async move {
					let started = Instant::now();
					let answer = match timeout(PROBE_TIMEOUT, well_known(&name, DEFAULT_PORT)).await
					{
						Ok(answer) => answer,
						Err(_) => Err(format!("no answer within {} s", PROBE_TIMEOUT.as_secs())),
					};
					let elapsed_ms = started.elapsed().as_millis() as u64;
					match &answer {
						Ok(url) => debug!(name, %url, elapsed_ms, "tsgw well-known probe answered"),
						Err(outcome) => debug!(name, %outcome, elapsed_ms, "tsgw well-known probe"),
					}
					answer.ok()
				}
				.boxed()
			}),
		}
	}
}

/// A DNS answer, the failure logged (no record at most names is normal).
fn dns_answer<T>(
	name: &str,
	kind: &str,
	started: Instant,
	lookup: Result<T, hickory_resolver::net::NetError>,
) -> Option<T> {
	let elapsed_ms = started.elapsed().as_millis() as u64;
	match lookup {
		Ok(lookup) => Some(lookup),
		Err(e) if e.is_no_records_found() => {
			debug!(name, kind, elapsed_ms, "no DNS record");
			None
		}
		Err(e) => {
			debug!(name, kind, elapsed_ms, error = %e, "DNS lookup failed");
			None
		}
	}
}

/// The URLs of the gateway of the server at `address` (as a bookmark has
/// it: host, host:port, an IP), best first: everything published, to be
/// tried in turn. Empty: none is published.
pub async fn gateways(address: &str) -> Vec<String> {
	gateways_with(address, Lookups::system()).await
}

async fn gateways_with(address: &str, lookups: Lookups) -> Vec<String> {
	let started = Instant::now();
	let host = host_of(address);
	let names = names(host);
	if names.is_empty() {
		debug!(address, "no gateway discovery for a server nickname");
		return Vec::new();
	}
	// The gateway's own answer is asked at every name while DNS is; only
	// the answer that counts is waited for.
	let mut probes = Probes::start(&names, &lookups);
	let srv = if host.parse::<IpAddr>().is_err() && host.contains('.') {
		from_srv(&names, &lookups).await
	} else {
		None
	};
	// The gateway's own answer comes over plain HTTP: not for a server that
	// publishes only TLS; at the name that publishes, or without records the
	// most specific name that answers.
	let (mut urls, well_known) = match &srv {
		None => match probes.first().await {
			Some((name, url)) => (vec![url.clone()], format!("{name}: {url}")),
			None => (Vec::new(), "none".to_owned()),
		},
		Some(found) if found.urls.iter().any(|url| url.starts_with("ws://")) => {
			let mut urls = found.urls.clone();
			match probes.of(found.name).await {
				Some(url) => {
					let well_known = format!("{}: {url}", found.name);
					if !urls.contains(&url) {
						urls.push(url);
					}
					(urls, well_known)
				}
				None => (urls, "none".to_owned()),
			}
		}
		Some(found) => (found.urls.clone(), "not used (TLS only)".to_owned()),
	};
	urls.truncate(MAX_CANDIDATES);
	let (srv_name, srv_urls, txt) = match &srv {
		Some(found) => (found.name, found.urls.as_slice(), found.txt.as_slice()),
		None => ("", &[][..], &[][..]),
	};
	info!(
		address,
		names = ?names,
		srv_name,
		srv = ?srv_urls,
		txt = ?txt,
		%well_known,
		result = ?urls,
		elapsed_ms = started.elapsed().as_millis() as u64,
		"gateway discovery"
	);
	urls
}

/// The gateway's own answers being asked, one per name, most specific
/// first; those still asking when dropped are given up.
struct Probes<'a>(Vec<(&'a str, JoinHandle<Option<String>>)>);

impl<'a> Probes<'a> {
	fn start(names: &[&'a str], lookups: &Lookups) -> Self {
		Self(
			names
				.iter()
				.map(|name| (*name, tokio::spawn((lookups.probe)(name.to_string()))))
				.collect(),
		)
	}

	/// The answer at `name`.
	async fn of(&mut self, name: &str) -> Option<String> {
		let (_, probe) = self.0.iter_mut().find(|(n, _)| *n == name)?;
		probe.await.ok().flatten()
	}

	/// The answer of the most specific name that answers.
	async fn first(&mut self) -> Option<(&'a str, String)> {
		for (name, probe) in &mut self.0 {
			if let Some(url) = probe.await.ok().flatten() {
				return Some((*name, url));
			}
		}
		None
	}
}

impl Drop for Probes<'_> {
	fn drop(&mut self) {
		for (_, probe) in &self.0 {
			probe.abort();
		}
	}
}

/// The host of an address: without the port, the brackets of an IPv6
/// address and a final dot.
fn host_of(address: &str) -> &str {
	let address = address.trim();
	let host = match address.strip_prefix('[') {
		Some(rest) => rest.split(']').next().unwrap_or(rest),
		// One colon: host and port; more: an IPv6 address.
		None if address.matches(':').count() == 1 => address.split(':').next().unwrap_or(address),
		None => address,
	};
	host.trim_end_matches('.')
}

/// The names to look at, most specific first: a host name and its parent
/// domains down to the last two labels; an IP address or `localhost` alone;
/// nothing for a server nickname.
// ponytail: two labels as the floor instead of the public suffix list, as
// the voice resolver's TSDNS lookup; under `co.uk`-style suffixes this also
// asks the suffix.
fn names(host: &str) -> Vec<&str> {
	if host.parse::<IpAddr>().is_ok() || host == "localhost" {
		return vec![host];
	}
	std::iter::successors(Some(host), |name| name.split_once('.').map(|(_, parent)| parent))
		.take_while(|name| name.contains('.'))
		.collect()
}

/// What SRV records publish: at `name`, the most specific name with a
/// usable record.
struct Published<'a> {
	name: &'a str,
	/// TLS first, by priority.
	urls: Vec<String>,
	/// The TXT strings at the records' names.
	txt: Vec<String>,
}

/// The URLs from SRV records, every name and service asked at once, with
/// the TXT records there (the path; most names have none): those of the
/// most specific name with a usable record.
async fn from_srv<'a>(names: &[&'a str], lookups: &Lookups) -> Option<Published<'a>> {
	let queries: Vec<(&str, &str, String)> = names
		.iter()
		.flat_map(|name| {
			[
				(*name, "wss", format!("_tsgws._tcp.{name}.")),
				(*name, "ws", format!("_tsgw._tcp.{name}.")),
			]
		})
		.collect();
	let (answers, texts) = futures::join!(
		join_all(queries.iter().map(|(.., owner)| (lookups.srv)(owner.clone()))),
		join_all(queries.iter().map(|(.., owner)| (lookups.txt)(owner.clone()))),
	);
	let mut found: Vec<_> = queries
		.iter()
		.zip(answers)
		.zip(texts)
		.filter_map(|((query, mut records), txt)| {
			// A target of "." says there is no such service there.
			records.retain(|r| r.1 != ".");
			records.sort_by_key(|r| r.0);
			(!records.is_empty()).then_some((query, records, txt))
		})
		.collect();
	let name = found.first()?.0.0;
	found.retain(|((n, ..), ..)| *n == name);
	let mut published = Published { name, urls: Vec::new(), txt: Vec::new() };
	for ((_, scheme, _), records, txt) in found {
		let path = path_in(&txt);
		published.txt.extend(txt);
		for (_, target, port) in records {
			let url = format!("{scheme}://{}:{port}{path}", target.trim_end_matches('.'));
			if !published.urls.contains(&url) {
				published.urls.push(url);
			}
		}
	}
	Some(published)
}

/// The path in a SRV record's name's TXT strings: `path=…`, `/v1` without
/// one.
fn path_in(txt: &[String]) -> String {
	let mut path = txt
		.iter()
		.find_map(|s| s.strip_prefix("path=").map(str::to_owned))
		.unwrap_or_else(|| DEFAULT_PATH.to_owned());
	if !path.starts_with('/') {
		path.insert(0, '/');
	}
	path
}

/// tsgw's answer at `http://<host>:<port>/.well-known/tsgw`; else what
/// happened instead.
async fn well_known(host: &str, port: u16) -> Result<String, String> {
	let mut stream =
		TcpStream::connect((host, port)).await.map_err(|e| format!("cannot connect: {e}"))?;
	let authority =
		if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
	let request =
		format!("GET /.well-known/tsgw HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
	stream.write_all(request.as_bytes()).await.map_err(|e| format!("cannot ask: {e}"))?;
	let mut response = Vec::new();
	stream
		.take(16 * 1024)
		.read_to_end(&mut response)
		.await
		.map_err(|e| format!("no answer: {e}"))?;
	let response = String::from_utf8(response).map_err(|_| "not text".to_owned())?;
	let (head, body) = response.split_once("\r\n\r\n").ok_or("not HTTP")?;
	let status = head.split_whitespace().nth(1).unwrap_or_default();
	if status != "200" {
		return Err(format!("http {status}"));
	}
	let answer: serde_json::Value =
		serde_json::from_str(body).map_err(|_| "not a tsgw answer".to_owned())?;
	let url = answer.get("url").and_then(|url| url.as_str()).ok_or("no url in the answer")?;
	if url.starts_with("ws://") || url.starts_with("wss://") {
		Ok(url.to_owned())
	} else {
		Err(format!("not a gateway URL: {url}"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// DNS answers from these lists only: SRV `(name, priority, target,
	/// port)`, TXT `(name, string)`; no gateway answers for itself.
	fn fake(srv: &[(&str, u16, &str, u16)], txt: &[(&str, &str)]) -> Lookups {
		let srv: Vec<(String, u16, String, u16)> =
			srv.iter().map(|(n, p, t, port)| (n.to_string(), *p, t.to_string(), *port)).collect();
		let txt: Vec<(String, String)> =
			txt.iter().map(|(n, s)| (n.to_string(), s.to_string())).collect();
		Lookups {
			srv: Arc::new(move |name| {
				let found =
					srv.iter().filter(|r| r.0 == name).map(|r| (r.1, r.2.clone(), r.3)).collect();
				futures::future::ready(found).boxed()
			}),
			txt: Arc::new(move |name| {
				let found = txt.iter().filter(|r| r.0 == name).map(|r| r.1.clone()).collect();
				futures::future::ready(found).boxed()
			}),
			probe: Arc::new(|_| futures::future::ready(None).boxed()),
		}
	}

	/// `lookups` where the gateway at a name answers `(name, url)`.
	fn answering(mut lookups: Lookups, answers: &[(&str, &str)]) -> Lookups {
		let answers: Vec<(String, String)> =
			answers.iter().map(|(n, u)| (n.to_string(), u.to_string())).collect();
		lookups.probe = Arc::new(move |name| {
			let url = answers.iter().find(|a| a.0 == name).map(|a| a.1.clone());
			futures::future::ready(url).boxed()
		});
		lookups
	}

	/// `lookups` asking the real `/.well-known/tsgw` at `port`.
	fn probing(mut lookups: Lookups, port: u16) -> Lookups {
		lookups.probe = Arc::new(move |name| {
			async move { timeout(PROBE_TIMEOUT, well_known(&name, port)).await.ok()?.ok() }.boxed()
		});
		lookups
	}

	/// A port nothing listens on.
	fn closed_port() -> u16 {
		std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
	}

	#[tokio::test]
	async fn finds_the_gateway_at_the_host_or_its_domain() {
		let lookups = fake(&[("_tsgw._tcp.example.test.", 0, "gw.example.test.", 7788)], &[]);
		for address in ["ts.example.test", "ts.example.test:9987", "example.test"] {
			assert_eq!(
				gateways_with(address, lookups.clone()).await,
				["ws://gw.example.test:7788/v1"],
				"{address}"
			);
		}
		// The parent's parent is not the domain any more.
		assert!(gateways_with("example", lookups).await.is_empty());
	}

	/// All of the most specific name's gateways, TLS first, by priority; a
	/// parent domain's only for a host without its own.
	#[tokio::test]
	async fn the_most_specific_name_wins_with_all_its_gateways() {
		let lookups = fake(
			&[
				("_tsgws._tcp.example.test.", 0, "domain.example.test.", 443),
				("_tsgw._tcp.ts.example.test.", 0, "plain.example.test.", 7788),
				("_tsgws._tcp.ts.example.test.", 5, "backup.example.test.", 443),
				("_tsgws._tcp.ts.example.test.", 1, "tls.example.test.", 8443),
			],
			&[("_tsgws._tcp.ts.example.test.", "path=/tsgw/v1")],
		);
		assert_eq!(
			gateways_with("ts.example.test", lookups.clone()).await,
			[
				"wss://tls.example.test:8443/tsgw/v1",
				"wss://backup.example.test:443/tsgw/v1",
				"ws://plain.example.test:7788/v1",
			]
		);
		assert_eq!(
			gateways_with("voice.example.test", lookups).await,
			["wss://domain.example.test:443/v1"]
		);
	}

	/// A TLS proxy in front of tsgw and tsgw itself published next to the
	/// server (and again at the domain): TLS first, the plain one after it,
	/// once; the gateway's own answer is the plain one again.
	#[tokio::test]
	async fn a_tls_proxy_falls_back_to_the_published_plain_gateway() {
		let lookups = answering(
			fake(
				&[
					("_tsgws._tcp.ts.example.test.", 0, "gw.example.test.", 443),
					("_tsgw._tcp.ts.example.test.", 0, "ts.example.test.", 7788),
					("_tsgw._tcp.example.test.", 0, "ts.example.test.", 7788),
				],
				&[],
			),
			&[("ts.example.test", "ws://ts.example.test:7788/v1")],
		);
		assert_eq!(
			gateways_with("ts.example.test", lookups).await,
			["wss://gw.example.test:443/v1", "ws://ts.example.test:7788/v1"]
		);
	}

	/// A gateway at the domain fronts another server than one of its hosts
	/// that publishes its own: never a fallback for it, nor the domain's own
	/// answer.
	#[tokio::test]
	async fn another_servers_gateway_is_no_fallback() {
		let lookups = answering(
			fake(
				&[
					("_tsgws._tcp.ts1.example.test.", 0, "gw1.example.test.", 443),
					("_tsgw._tcp.ts1.example.test.", 0, "ts1.example.test.", 7788),
					("_tsgws._tcp.example.test.", 0, "gw-main.example.test.", 443),
				],
				&[],
			),
			&[("example.test", "ws://main.example.test:7788/v1")],
		);
		assert_eq!(
			gateways_with("ts1.example.test", lookups).await,
			["wss://gw1.example.test:443/v1", "ws://ts1.example.test:7788/v1"]
		);
	}

	#[tokio::test]
	async fn the_gateways_own_answer_comes_last() {
		let lookups = answering(
			fake(&[("_tsgw._tcp.ts.example.test.", 0, "plain.example.test.", 7788)], &[]),
			&[("ts.example.test", "wss://gw.example.test/v1")],
		);
		assert_eq!(
			gateways_with("ts.example.test", lookups).await,
			["ws://plain.example.test:7788/v1", "wss://gw.example.test/v1"]
		);
	}

	/// The gateway's plain answer is asked with DNS, but not used.
	#[tokio::test]
	async fn a_tls_only_publication_is_not_widened() {
		let lookups = answering(
			fake(&[("_tsgws._tcp.ts.example.test.", 0, "gw.example.test.", 443)], &[]),
			&[("ts.example.test", "ws://ts.example.test:7788/v1")],
		);
		assert_eq!(
			gateways_with("ts.example.test", lookups).await,
			["wss://gw.example.test:443/v1"]
		);
	}

	/// `lookups` whose DNS answers take `dns` and the gateway's own `probe`.
	fn slow(lookups: Lookups, dns: Duration, probe: Duration) -> Lookups {
		let Lookups { srv, txt, probe: answer } = lookups;
		Lookups {
			srv: Arc::new(move |name| {
				let found = srv(name);
				async move {
					tokio::time::sleep(dns).await;
					found.await
				}
				.boxed()
			}),
			txt: Arc::new(move |name| {
				let found = txt(name);
				async move {
					tokio::time::sleep(dns).await;
					found.await
				}
				.boxed()
			}),
			probe: Arc::new(move |name| {
				let found = answer(name);
				async move {
					tokio::time::sleep(probe).await;
					found.await
				}
				.boxed()
			}),
		}
	}

	/// SRV, TXT and the gateway's own answer are asked at once: a slow
	/// gateway and slow DNS cost the slower of them, not the sum; an answer
	/// that is not used is not waited for.
	#[tokio::test(start_paused = true)]
	async fn everything_is_asked_at_once() {
		let published = |srv: &[(&str, u16, &str, u16)]| {
			answering(
				fake(srv, &[("_tsgw._tcp.ts.example.test.", "path=/gw")]),
				&[("ts.example.test", "wss://proxy.example.test/v1")],
			)
		};
		let plain = [("_tsgw._tcp.ts.example.test.", 0, "ts.example.test.", 7788)];
		let (dns, probe) = (Duration::from_millis(400), Duration::from_millis(2500));
		let started = tokio::time::Instant::now();
		assert_eq!(
			gateways_with("ts.example.test", slow(published(&plain), dns, probe)).await,
			["ws://ts.example.test:7788/gw", "wss://proxy.example.test/v1"]
		);
		assert_eq!(started.elapsed().as_millis(), 2500);
		let started = tokio::time::Instant::now();
		assert_eq!(
			gateways_with("ts.example.test", slow(published(&plain), probe, dns)).await,
			["ws://ts.example.test:7788/gw", "wss://proxy.example.test/v1"]
		);
		assert_eq!(started.elapsed().as_millis(), 2500);
		// TLS only: the gateway's plain answer is not waited for.
		let tls = [("_tsgws._tcp.ts.example.test.", 0, "gw.example.test.", 443)];
		let started = tokio::time::Instant::now();
		assert_eq!(
			gateways_with("ts.example.test", slow(published(&tls), dns, probe)).await,
			["wss://gw.example.test:443/v1"]
		);
		assert_eq!(started.elapsed().as_millis(), 400);
	}

	/// Without records, the most specific name's gateway is waited for
	/// before a parent domain's answer is taken.
	#[tokio::test(start_paused = true)]
	async fn without_records_the_most_specific_answer_wins() {
		let mut lookups = fake(&[], &[]);
		lookups.probe = Arc::new(|name| {
			async move {
				match name.as_str() {
					"ts.example.test" => {
						tokio::time::sleep(Duration::from_secs(2)).await;
						Some("ws://ts.example.test:7788/v1".to_owned())
					}
					_ => Some("ws://example.test:7788/v1".to_owned()),
				}
			}
			.boxed()
		});
		let started = tokio::time::Instant::now();
		assert_eq!(
			gateways_with("ts.example.test", lookups).await,
			["ws://ts.example.test:7788/v1"]
		);
		assert_eq!(started.elapsed().as_millis(), 2000);
	}

	#[tokio::test]
	async fn a_record_that_says_no_service_is_skipped() {
		let lookups = fake(
			&[
				("_tsgw._tcp.ts.example.test.", 0, ".", 0),
				("_tsgw._tcp.example.test.", 0, "gw.example.test.", 80),
			],
			&[("_tsgw._tcp.example.test.", "path=ws")],
		);
		assert_eq!(gateways_with("ts.example.test", lookups).await, ["ws://gw.example.test:80/ws"]);
	}

	/// Without records the gateway next to the server answers at its port,
	/// as tsgw does (`/.well-known/tsgw`).
	#[tokio::test]
	async fn without_records_the_gateway_itself_is_asked() {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(async move {
			loop {
				let (mut socket, _) = listener.accept().await.unwrap();
				let mut request = [0; 1024];
				let n = socket.read(&mut request).await.unwrap();
				let request = String::from_utf8_lossy(&request[..n]).into_owned();
				let response = if request.starts_with("GET /.well-known/tsgw ") {
					let body = r#"{"url":"wss://gw.example.test/v1"}"#;
					format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len())
				} else {
					"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned()
				};
				socket.write_all(response.as_bytes()).await.unwrap();
			}
		});
		let lookups = probing(fake(&[], &[]), port);
		for address in ["127.0.0.1", "127.0.0.1:9987", "localhost"] {
			assert_eq!(
				gateways_with(address, lookups.clone()).await,
				["wss://gw.example.test/v1"],
				"{address}"
			);
		}
		// Nothing there.
		let nothing = probing(fake(&[], &[]), closed_port());
		assert!(gateways_with("127.0.0.1", nothing).await.is_empty());
	}

	#[test]
	fn names_walk_up_to_the_domain() {
		assert_eq!(
			names(host_of("ts.sub.example.test:9987")),
			["ts.sub.example.test", "sub.example.test", "example.test"]
		);
		assert_eq!(names(host_of("[::1]:9987")), ["::1"]);
		assert_eq!(names(host_of("::1")), ["::1"]);
		assert_eq!(names(host_of("example.test.")), ["example.test"]);
		// A server nickname is resolved by TeamSpeak, not in DNS.
		assert!(names(host_of("nickname")).is_empty());
	}
}
