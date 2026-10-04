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
//! TLS first at the same name; `<path>` is `path=…` from a TXT record at the
//! SRV record's name, `/v1` without one. Without any such record, as TSDNS
//! has its well-known port, the gateway itself is asked at tsgw's default
//! port: `http://<name>:7788/.well-known/tsgw` answers `{"url": "…"}`. For an
//! IP address (or `localhost`) only that is tried. What an admin publishes is
//! in `docs/gateway-admin.md`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, join_all};
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{CLOUDFLARE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RData;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// The port tsgw listens on by default, where it is asked for its URL.
pub const DEFAULT_PORT: u16 = 7788;
/// Where tsgw serves its WebSocket.
const DEFAULT_PATH: &str = "/v1";
/// Give up on asking one name's gateway after this long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

type SrvFn = dyn Fn(String) -> BoxFuture<'static, Vec<(u16, String, u16)>> + Send + Sync;
type TxtFn = dyn Fn(String) -> BoxFuture<'static, Vec<String>> + Send + Sync;

/// Where [`gateway`] gets its answers from; the tests put fakes in.
#[derive(Clone)]
struct Lookups {
	/// The SRV records at a name: priority, target, port.
	srv: Arc<SrvFn>,
	/// The strings of the TXT records at a name.
	txt: Arc<TxtFn>,
	/// Where a gateway is asked for its URL.
	probe_port: u16,
}

impl Lookups {
	fn system() -> Self {
		// The system's resolver, else (Android) a public one, as the voice
		// connection's resolver does.
		let resolver = TokioResolver::builder_tokio().and_then(|b| b.build()).or_else(|_| {
			TokioResolver::builder_with_config(
				ResolverConfig::udp_and_tcp(&CLOUDFLARE),
				TokioRuntimeProvider::default(),
			)
			.build()
		});
		let resolver = resolver.ok();
		let txt_resolver = resolver.clone();
		Self {
			srv: Arc::new(move |name| {
				let resolver = resolver.clone();
				async move {
					let lookup = resolver?.srv_lookup(name).await.ok()?;
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
					let lookup = resolver?.txt_lookup(name).await.ok()?;
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
			probe_port: DEFAULT_PORT,
		}
	}
}

/// The URL of the gateway of the server at `address` (as a bookmark has it:
/// host, host:port, an IP), if one is published.
pub async fn gateway(address: &str) -> Option<String> {
	gateway_with(address, Lookups::system()).await
}

async fn gateway_with(address: &str, lookups: Lookups) -> Option<String> {
	let host = host_of(address);
	let names = names(host);
	if names.is_empty() {
		return None;
	}
	if host.parse::<IpAddr>().is_err()
		&& host.contains('.')
		&& let Some(url) = from_srv(&names, &lookups).await
	{
		return Some(url);
	}
	from_well_known(&names, lookups.probe_port).await
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

/// The URL from SRV records, every name and service asked at once.
async fn from_srv(names: &[&str], lookups: &Lookups) -> Option<String> {
	let queries: Vec<(&str, String)> = names
		.iter()
		.flat_map(|name| {
			[("wss", format!("_tsgws._tcp.{name}.")), ("ws", format!("_tsgw._tcp.{name}."))]
		})
		.collect();
	let answers = join_all(queries.iter().map(|(_, owner)| (lookups.srv)(owner.clone()))).await;
	// The first (most specific, TLS first) name with a usable record.
	let ((scheme, owner), (_, target, port)) =
		queries.iter().zip(answers).find_map(|(q, records)| {
			// A target of "." says there is no such service there.
			let best = records.into_iter().filter(|r| r.1 != ".").min_by_key(|r| r.0)?;
			Some((q, best))
		})?;
	let mut path = (lookups.txt)(owner.clone())
		.await
		.into_iter()
		.find_map(|s| s.strip_prefix("path=").map(str::to_owned))
		.unwrap_or_else(|| DEFAULT_PATH.to_owned());
	if !path.starts_with('/') {
		path.insert(0, '/');
	}
	Some(format!("{scheme}://{}:{port}{path}", target.trim_end_matches('.')))
}

/// The URL a gateway gives for itself at one of `names`, all asked at once;
/// the most specific that answers wins.
async fn from_well_known(names: &[&str], port: u16) -> Option<String> {
	let answers =
		join_all(names.iter().map(|name| timeout(PROBE_TIMEOUT, well_known(name, port)))).await;
	answers.into_iter().find_map(|answer| answer.ok().flatten())
}

/// tsgw's answer at `http://<host>:<port>/.well-known/tsgw`.
async fn well_known(host: &str, port: u16) -> Option<String> {
	let mut stream = TcpStream::connect((host, port)).await.ok()?;
	let authority =
		if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
	let request =
		format!("GET /.well-known/tsgw HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
	stream.write_all(request.as_bytes()).await.ok()?;
	let mut response = Vec::new();
	stream.take(16 * 1024).read_to_end(&mut response).await.ok()?;
	let response = String::from_utf8(response).ok()?;
	let (head, body) = response.split_once("\r\n\r\n")?;
	if head.split_whitespace().nth(1) != Some("200") {
		return None;
	}
	let answer: serde_json::Value = serde_json::from_str(body).ok()?;
	let url = answer.get("url")?.as_str()?;
	(url.starts_with("ws://") || url.starts_with("wss://")).then(|| url.to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// DNS answers from these lists only: SRV `(name, priority, target,
	/// port)`, TXT `(name, string)`.
	fn fake(srv: &[(&str, u16, &str, u16)], txt: &[(&str, &str)], probe_port: u16) -> Lookups {
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
			probe_port,
		}
	}

	/// A port nothing listens on.
	fn closed_port() -> u16 {
		std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
	}

	#[tokio::test]
	async fn finds_the_gateway_at_the_host_or_its_domain() {
		let lookups = fake(&[("_tsgw._tcp.example.test.", 0, "gw.example.test.", 7788)], &[], 0);
		for address in ["ts.example.test", "ts.example.test:9987", "example.test"] {
			assert_eq!(
				gateway_with(address, lookups.clone()).await.as_deref(),
				Some("ws://gw.example.test:7788/v1"),
				"{address}"
			);
		}
		// The parent's parent is not the domain any more.
		assert_eq!(gateway_with("example", lookups).await, None);
	}

	#[tokio::test]
	async fn the_most_specific_name_wins_then_tls() {
		let lookups = fake(
			&[
				("_tsgws._tcp.example.test.", 0, "domain.example.test.", 443),
				("_tsgw._tcp.ts.example.test.", 0, "plain.example.test.", 7788),
				("_tsgws._tcp.ts.example.test.", 5, "backup.example.test.", 443),
				("_tsgws._tcp.ts.example.test.", 1, "tls.example.test.", 8443),
			],
			&[("_tsgws._tcp.ts.example.test.", "path=/tsgw/v1")],
			0,
		);
		assert_eq!(
			gateway_with("ts.example.test", lookups.clone()).await.as_deref(),
			Some("wss://tls.example.test:8443/tsgw/v1")
		);
		assert_eq!(
			gateway_with("voice.example.test", lookups).await.as_deref(),
			Some("wss://domain.example.test:443/v1")
		);
	}

	#[tokio::test]
	async fn a_record_that_says_no_service_is_skipped() {
		let lookups = fake(
			&[
				("_tsgw._tcp.ts.example.test.", 0, ".", 0),
				("_tsgw._tcp.example.test.", 0, "gw.example.test.", 80),
			],
			&[("_tsgw._tcp.example.test.", "path=ws")],
			closed_port(),
		);
		assert_eq!(
			gateway_with("ts.example.test", lookups).await.as_deref(),
			Some("ws://gw.example.test:80/ws")
		);
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
		let lookups = fake(&[], &[], port);
		for address in ["127.0.0.1", "127.0.0.1:9987", "localhost"] {
			assert_eq!(
				gateway_with(address, lookups.clone()).await.as_deref(),
				Some("wss://gw.example.test/v1"),
				"{address}"
			);
		}
		// Nothing there.
		assert_eq!(gateway_with("127.0.0.1", fake(&[], &[], closed_port())).await, None);
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
