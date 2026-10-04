//! Resolve TeamSpeak server addresses of any kind.
// Changes with TeamSpeak client 3.1:
// https://support.teamspeakusa.com/index.php?/Knowledgebase/Article/View/332

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::str;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::prelude::*;
use hickory_net::proto::rr::RData;
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{CLOUDFLARE, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use rand::RngExt;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{self, TcpStream};
use tokio::time::{Duration, timeout};
use tracing::{debug, instrument, warn};

const DEFAULT_PORT: u16 = 9987;
/// Port a TSDNS server listens on.
const TSDNS_PORT: u16 = 41144;
const DNS_PREFIX_TCP: &str = "_tsdns._tcp.";
const DNS_PREFIX_UDP: &str = "_ts3._udp.";
const NICKNAME_LOOKUP_ADDRESS: &str = "https://named.myteamspeak.com/lookup";
/// Give up on one way of resolving after this long.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Give up on one TSDNS server (connecting and asking) after this long, so a
/// firewall that drops the port does not hold up connecting.
const TSDNS_TIMEOUT: Duration = Duration::from_secs(3);

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
	#[error("Failed to create resolver: {0}")]
	CreateResolver(#[source] hickory_net::NetError),
	#[error("Invalid IPv4 address")]
	InvalidIp4Address,
	#[error("Invalid IPv6 address")]
	InvalidIp6Address,
	#[error("Invalid IP address")]
	InvalidIpAddress,
	#[error("Not a valid nickname")]
	InvalidNickname,
	#[error("Failed to parse port: {0}")]
	InvalidPort(#[source] std::num::ParseIntError),
	#[error("Failed to contact {0} server: {1}")]
	Io(&'static str, #[source] std::io::Error),
	#[error("Failed to parse url: {0}")]
	NicknameParseUrl(#[source] url::ParseError),
	#[error("Failed to resolve nickname: {0}")]
	NicknameResolve(#[source] reqwest::Error),
	#[error("Failed to resolve hostname: {0}")]
	ResolveHost(#[source] tokio::io::Error),
	#[error("Failed to get SRV record")]
	SrvLookup(#[source] hickory_net::NetError),
	#[error("tsdns did not return an ip address but {0:?}")]
	TsdnsAddressInvalidResponse(String),
	#[error("tsdns server does not know the address")]
	TsdnsAddressNotFound,
	#[error("Failed to parse tsdns response: {0}")]
	TsdnsParseResponse(#[source] std::str::Utf8Error),
}

#[derive(Debug, PartialEq, Eq)]
enum ParseIpResult<'a> {
	Addr(SocketAddr),
	Other(&'a str, Option<u16>),
}

/// Beware that this may be slow because it tries all available methods.
///
/// The methods start at once and each is given up after a while (as the
/// official client does); their addresses come out in this order:
/// 1. If the address is an ip, the ip is returned
/// 1. Server nicknames are resolved by a http request to TeamSpeak
/// 1. The SRV record at `_ts3._udp.<address>`
/// 1. A TSDNS server: the ones the SRV records at `_tsdns._tcp.address.tld`
///    name, e.g. when the address is `ts3.subdomain.from.com`, the SRV record
///    at `_tsdns._tcp.from.com` is requested; without such a record, the ones
///    on port 41144 at the address's parent domains (`subdomain.from.com`,
///    `from.com`). The first that knows the address answers.
/// 1. Directly resolve the address to an ip address, port 9987
///
/// If a port is given with `:port`, it overwrites the port of every address
/// found. IPv6 addresses are put in square brackets when a port is present:
/// `[::1]:9987`
#[instrument]
pub fn resolve(address: String) -> impl Stream<Item = Result<SocketAddr>> {
	resolve_with(address, Lookups::system())
}

type SrvFn = dyn Fn(String) -> BoxFuture<'static, Result<Vec<(String, u16)>>> + Send + Sync;
type HostFn = dyn Fn(String, u16) -> BoxFuture<'static, Result<Vec<SocketAddr>>> + Send + Sync;

/// Where [`resolve`] gets its answers from; the tests put fakes in.
#[derive(Clone)]
struct Lookups {
	/// The targets and ports of the SRV records at a name, in the order to
	/// try them.
	srv: Arc<SrvFn>,
	/// The addresses of a host name, with this port.
	host: Arc<HostFn>,
	tsdns_port: u16,
	tsdns_timeout: Duration,
}

impl Lookups {
	fn system() -> Self {
		Self {
			srv: Arc::new(|name| srv_targets(name).boxed()),
			host: Arc::new(|host, port| {
				async move {
					Ok(net::lookup_host((host.as_str(), port))
						.await
						.map_err(Error::ResolveHost)?
						.collect())
				}
				.boxed()
			}),
			tsdns_port: TSDNS_PORT,
			tsdns_timeout: TSDNS_TIMEOUT,
		}
	}
}

fn resolve_with(address: String, lookups: Lookups) -> impl Stream<Item = Result<SocketAddr>> {
	debug!("Starting resolve");
	let (host, port) = match parse_ip(&address) {
		Ok(ParseIpResult::Addr(res)) => {
			return stream::once(future::ok(res)).left_stream();
		}
		Ok(ParseIpResult::Other(host, port)) => (host.trim_end_matches('.').to_string(), port),
		Err(res) => return stream::once(future::err(res)).left_stream(),
	};
	if let Some(port) = port {
		debug!(port, "Found port");
	}

	let mut steps: Vec<BoxFuture<'static, Result<Vec<SocketAddr>>>> = Vec::new();
	if !host.contains('.') && host != "localhost" {
		// Could be a server nickname
		steps.push(resolve_nickname(host.clone()).try_collect().boxed());
	}
	steps.push(srv_addresses(lookups.clone(), format!("{DNS_PREFIX_UDP}{host}.")).boxed());
	steps.push(tsdns(lookups.clone(), host.clone()).boxed());
	// Interpret as normal address
	steps.push((lookups.host)(host, DEFAULT_PORT));

	let count = steps.len();
	stream::iter(steps)
		.map(|step| timeout(STEP_TIMEOUT, step))
		// All at once, the addresses in the order of the steps.
		.buffered(count)
		.flat_map(|result| {
			stream::iter(match result {
				Ok(Ok(addresses)) => addresses,
				Ok(Err(error)) => {
					debug!(%error, "Resolver failed in one step");
					Vec::new()
				}
				Err(_) => {
					debug!("Resolver step timed out");
					Vec::new()
				}
			})
		})
		.map(move |mut addr| {
			if let Some(port) = port {
				// A port the user gave wins.
				addr.set_port(port);
			}
			Ok(addr)
		})
		.right_stream()
}

/// The addresses of the targets of the SRV records at `name`, in their order.
async fn srv_addresses(lookups: Lookups, name: String) -> Result<Vec<SocketAddr>> {
	let mut addresses = Vec::new();
	for (target, port) in (lookups.srv)(name).await? {
		match (lookups.host)(target, port).await {
			Ok(found) => addresses.extend(found),
			Err(error) => debug!(%error, "SRV target not found"),
		}
	}
	Ok(addresses)
}

/// Where a TSDNS server for `host` may be, most specific first: the domains
/// above `host` down to the last two labels, or `host` itself when it has
/// only two (`ts.sub.example.com`: `sub.example.com`, `example.com`).
// ponytail: two labels as the floor instead of the public suffix list; under
// a `co.uk`-style suffix this also asks the suffix.
fn tsdns_domains(host: &str) -> Vec<&str> {
	fn parent(name: &str) -> Option<&str> { name.split_once('.').map(|(_, parent)| parent) }
	let mut domains: Vec<&str> =
		std::iter::successors(parent(host), |d| parent(d)).filter(|d| d.contains('.')).collect();
	if domains.is_empty() && host.contains('.') {
		domains.push(host);
	}
	domains
}

/// The address a TSDNS server has for `host`. The servers are the ones the
/// SRV records at `_tsdns._tcp.<domain>` name or, without such records (as
/// the official client does), the ones on the TSDNS port at `host`'s domain
/// and its parents. They are asked at once; the first in that order that
/// knows `host` answers.
async fn tsdns(lookups: Lookups, host: String) -> Result<Vec<SocketAddr>> {
	let domains = tsdns_domains(&host);
	let Some(domain) = domains.last() else { return Ok(Vec::new()) };
	let mut servers = srv_addresses(lookups.clone(), format!("{DNS_PREFIX_TCP}{domain}."))
		.await
		.unwrap_or_default();
	if servers.is_empty() {
		let found = future::join_all(
			domains.iter().map(|d| (lookups.host)(d.to_string(), lookups.tsdns_port)),
		)
		.await;
		servers = found.into_iter().filter_map(|r| r.ok()).flatten().collect();
	}
	if servers.is_empty() {
		return Ok(Vec::new());
	}
	let count = servers.len();
	let mut answers = stream::iter(servers)
		.map(|server| timeout(lookups.tsdns_timeout, resolve_tsdns(server, &host)))
		.buffered(count);
	let mut error = Error::TsdnsAddressNotFound;
	while let Some(answer) = answers.next().await {
		match answer {
			Ok(Ok(address)) => return Ok(vec![address]),
			Ok(Err(e)) => error = e,
			Err(_) => error = Error::Io("tsdns", std::io::ErrorKind::TimedOut.into()),
		}
	}
	Err(error)
}

// Windows for some reason automatically adds a link-local address to the dns
// resolver. These addresses are usually not reachable and should be filtered out.
// See: https://superuser.com/questions/638566/strange-value-in-dns-shown-in-ipconfig
const FILTERED_IPS: &[IpAddr] = &[
	IpAddr::V6(Ipv6Addr::new(0xfec0, 0, 0, 0xffff, 0, 0, 0, 1)),
	IpAddr::V6(Ipv6Addr::new(0xfec0, 0, 0, 0xffff, 0, 0, 0, 2)),
	IpAddr::V6(Ipv6Addr::new(0xfec0, 0, 0, 0xffff, 0, 0, 0, 3)),
];

fn create_resolver() -> Result<TokioResolver> {
	let (config, options) = match hickory_resolver::system_conf::read_system_conf() {
		Ok((mut config, options)) => {
			config.name_servers.retain(|ns| !FILTERED_IPS.contains(&ns.ip));
			(config, options)
		}
		Err(error) => {
			warn!(%error, "Failed to use system dns resolver config");
			// Fallback
			(ResolverConfig::udp_and_tcp(&CLOUDFLARE), ResolverOpts::default())
		}
	};
	let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
	*builder.options_mut() = options;
	builder.build().map_err(Error::CreateResolver)
}

fn parse_ip(address: &str) -> Result<ParseIpResult<'_>> {
	let mut addr = address;
	let mut port = None;
	if let Some(pos) = address.rfind(':') {
		// Either with port or IPv6 address
		if address.find(':').unwrap() == pos {
			// Port is appended
			addr = &address[..pos];
			port = Some(&address[pos + 1..]);
			if addr.chars().all(|c| c.is_ascii_digit() || c == '.') {
				// IPv4 address
				return Ok(ParseIpResult::Addr(
					std::net::ToSocketAddrs::to_socket_addrs(address)
						.map_err(|_| Error::InvalidIp4Address)?
						.next()
						.ok_or(Error::InvalidIp4Address)?,
				));
			}
		} else if let Some(pos_bracket) = address.rfind(']') {
			if pos_bracket < pos {
				// IPv6 address and port
				return Ok(ParseIpResult::Addr(
					std::net::ToSocketAddrs::to_socket_addrs(address)
						.map_err(|_| Error::InvalidIp6Address)?
						.next()
						.ok_or(Error::InvalidIp6Address)?,
				));
			} else if pos_bracket == address.len() - 1 && address.starts_with('[') {
				// IPv6 address
				return Ok(ParseIpResult::Addr(
					std::net::ToSocketAddrs::to_socket_addrs(&(
						&address[1..pos_bracket],
						DEFAULT_PORT,
					))
					.map_err(|_| Error::InvalidIp6Address)?
					.next()
					.ok_or(Error::InvalidIp6Address)?,
				));
			} else {
				return Err(Error::InvalidIpAddress);
			}
		} else {
			// IPv6 address
			return Ok(ParseIpResult::Addr(
				std::net::ToSocketAddrs::to_socket_addrs(&(address, DEFAULT_PORT))
					.map_err(|_| Error::InvalidIp6Address)?
					.next()
					.ok_or(Error::InvalidIp6Address)?,
			));
		}
	} else if address.chars().all(|c| c.is_ascii_digit() || c == '.') {
		// IPv4 address
		return Ok(ParseIpResult::Addr(
			std::net::ToSocketAddrs::to_socket_addrs(&(address, DEFAULT_PORT))
				.map_err(|_| Error::InvalidIp4Address)?
				.next()
				.ok_or(Error::InvalidIp4Address)?,
		));
	}
	let port = if let Some(port) = port.map(|p| p.parse().map_err(Error::InvalidPort)) {
		Some(port?)
	} else {
		None
	};
	Ok(ParseIpResult::Other(addr, port))
}

pub fn resolve_nickname(nickname: String) -> impl Stream<Item = Result<SocketAddr>> {
	stream::once(async {
		let nickname = nickname;
		let url =
			reqwest::Url::parse_with_params(NICKNAME_LOOKUP_ADDRESS, Some(("name", &nickname)))
				.map_err(Error::NicknameParseUrl)?;
		let body = reqwest::get(url)
			.await
			.map_err(Error::NicknameResolve)?
			.error_for_status()
			.map_err(Error::NicknameResolve)?
			.text()
			.await
			.map_err(Error::NicknameResolve)?;
		let addrs = body
			.split(&['\r', '\n'][..])
			.filter(|s| !s.is_empty())
			.map(|s| Result::<_>::Ok(s.to_string()))
			.collect::<Vec<_>>();

		Result::<_>::Ok(
			stream::iter(addrs)
				.and_then(|addr| async move {
					match parse_ip(&addr)? {
						ParseIpResult::Addr(a) => Ok(stream::once(future::ok(a)).left_stream()),
						ParseIpResult::Other(a, p) => {
							let addrs = net::lookup_host((a, p.unwrap_or(DEFAULT_PORT)))
								.await
								.map_err(Error::ResolveHost)?
								.collect::<Vec<_>>();
							Ok(stream::iter(addrs).map(Result::<_>::Ok).right_stream())
						}
					}
				})
				.try_flatten(),
		)
	})
	.try_flatten()
}

pub async fn resolve_tsdns<A: net::ToSocketAddrs>(server: A, addr: &str) -> Result<SocketAddr> {
	let mut stream = TcpStream::connect(server).await.map_err(|e| Error::Io("tsdns", e))?;
	stream.write_all(addr.as_bytes()).await.map_err(|e| Error::Io("tsdns", e))?;
	let mut data = Vec::new();
	stream.read_to_end(&mut data).await.map_err(|e| Error::Io("tsdns", e))?;

	let addr = str::from_utf8(&data).map_err(Error::TsdnsParseResponse)?.trim();
	if addr.starts_with("404") {
		return Err(Error::TsdnsAddressNotFound);
	}
	match parse_ip(addr)? {
		ParseIpResult::Addr(a) => Ok(a),
		_ => Err(Error::TsdnsAddressInvalidResponse(addr.to_string())),
	}
}

/// The targets and ports of the SRV records at `name`, in the order to try
/// them.
async fn srv_targets(name: String) -> Result<Vec<(String, u16)>> {
	let lookup = create_resolver()?.srv_lookup(name).await.map_err(Error::SrvLookup)?;
	let records = lookup
		.answers()
		.iter()
		.filter_map(|r| match &r.data {
			RData::SRV(srv) => Some((srv.priority, srv.weight, (srv.target.to_ascii(), srv.port))),
			_ => None,
		})
		.collect();
	Ok(order_srv(records))
}

/// Order SRV records `(priority, weight, record)` as RFC 2782 says: lowest
/// priority first, and within one priority at random by weight, where weight
/// 0 is picked only rarely before the others.
fn order_srv<T>(mut records: Vec<(u16, u16, T)>) -> Vec<T> {
	// The weight 0 records of a priority first: only a draw of 0 picks them.
	records.sort_by_key(|(priority, weight, _)| (*priority, *weight != 0));
	let mut ordered = Vec::with_capacity(records.len());
	while let Some(&(priority, ..)) = records.first() {
		let end = records.iter().position(|r| r.0 != priority).unwrap_or(records.len());
		let total: u32 = records[..end].iter().map(|r| u32::from(r.1)).sum();
		let mut draw = rand::rng().random_range(0..=total);
		let pick = records[..end]
			.iter()
			.position(|r| {
				if draw <= u32::from(r.1) {
					return true;
				}
				draw -= u32::from(r.1);
				false
			})
			.unwrap_or(0);
		ordered.push(records.remove(pick).2);
	}
	ordered
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::tests::create_logger;

	#[test]
	fn parse_ip_without_port() {
		let res = parse_ip("127.0.0.1");
		assert_eq!(
			res.unwrap(),
			ParseIpResult::Addr(format!("127.0.0.1:{}", DEFAULT_PORT).parse().unwrap())
		);
	}

	#[test]
	fn parse_ip_with_port() {
		let res = parse_ip("127.0.0.1:1");
		assert_eq!(res.unwrap(), ParseIpResult::Addr("127.0.0.1:1".parse().unwrap()));
	}

	#[test]
	fn parse_ip6_without_port() {
		let res = parse_ip("::");
		assert_eq!(
			res.unwrap(),
			ParseIpResult::Addr(format!("[::]:{}", DEFAULT_PORT).parse().unwrap())
		);
	}

	#[test]
	fn parse_ip6_without_port2() {
		let res = parse_ip("[::]");
		assert_eq!(
			res.unwrap(),
			ParseIpResult::Addr(format!("[::]:{}", DEFAULT_PORT).parse().unwrap())
		);
	}

	#[test]
	fn parse_ip6_with_port() {
		let res = parse_ip("[::]:1");
		assert_eq!(res.unwrap(), ParseIpResult::Addr("[::]:1".parse().unwrap()));
	}

	#[test]
	fn parse_ip_address_without_port() {
		assert_eq!(parse_ip("localhost").unwrap(), ParseIpResult::Other("localhost", None));
	}

	#[test]
	fn parse_ip_address_with_port() {
		assert_eq!(parse_ip("localhost:1").unwrap(), ParseIpResult::Other("localhost", Some(1)));
	}

	#[test]
	fn parse_ip_with_large_port() {
		assert!(parse_ip("127.0.0.1:65536").is_err());
	}

	#[tokio::test]
	async fn resolve_localhost() {
		create_logger();
		let res: Vec<_> = resolve("127.0.0.1".into()).map(|r| r.unwrap()).collect().await;
		let addr = format!("127.0.0.1:{}", DEFAULT_PORT).parse::<SocketAddr>().unwrap();
		assert_eq!(res.as_slice(), &[addr]);
	}

	#[tokio::test]
	#[ignore = "needs DNS and the internet"]
	async fn resolve_localhost2() {
		create_logger();
		let res: Vec<_> = resolve("localhost".into()).map(|r| r.unwrap()).collect().await;
		assert!(res.contains(&format!("127.0.0.1:{}", DEFAULT_PORT).parse().unwrap()));
	}

	#[tokio::test]
	#[ignore = "needs DNS and the internet"]
	async fn resolve_example() {
		create_logger();
		let res: Vec<_> = resolve("example.com".into()).map(|r| r.unwrap()).collect().await;
		assert!(!res.is_empty());
	}

	#[tokio::test]
	#[ignore = "needs DNS and the internet"]
	async fn resolve_splamy_de() {
		create_logger();

		let res: Vec<_> = tokio::time::timeout(
			Duration::from_secs(5),
			resolve("splamy.de".into()).map(|r| r.unwrap()).collect(),
		)
		.await
		.expect("Resolve takes unacceptable long");
		assert!(res.contains(&format!("37.120.179.68:{}", DEFAULT_PORT).parse().unwrap()));
	}

	#[tokio::test]
	#[ignore = "needs DNS and the internet"]
	async fn resolve_loc() {
		create_logger();
		let res: Vec<_> = resolve("loc".into()).map(|r| r.unwrap()).collect().await;
		assert!(res.contains(&format!("127.0.0.1:{}", DEFAULT_PORT).parse().unwrap()));
	}

	/// Lookups that never leave this machine: `hosts` maps a name to an ip
	/// (with the port asked for), `srv` an SRV name to its target and port.
	fn fake(hosts: &[(&str, &str)], srv: &[(&str, &str, u16)], tsdns_port: u16) -> Lookups {
		let hosts: Vec<(String, IpAddr)> =
			hosts.iter().map(|(name, ip)| (name.to_string(), ip.parse().unwrap())).collect();
		let srv: Vec<(String, String, u16)> = srv
			.iter()
			.map(|(name, target, port)| (name.to_string(), target.to_string(), *port))
			.collect();
		let missing = || Error::Io("dns", std::io::ErrorKind::NotFound.into());
		Lookups {
			srv: Arc::new(move |name| {
				let found: Vec<_> =
					srv.iter().filter(|r| r.0 == name).map(|r| (r.1.clone(), r.2)).collect();
				future::ready(if found.is_empty() { Err(missing()) } else { Ok(found) }).boxed()
			}),
			host: Arc::new(move |name, port| {
				let found: Vec<_> = hosts
					.iter()
					.filter(|h| h.0 == name)
					.map(|h| SocketAddr::new(h.1, port))
					.collect();
				future::ready(if found.is_empty() { Err(missing()) } else { Ok(found) }).boxed()
			}),
			tsdns_port,
			tsdns_timeout: Duration::from_millis(500),
		}
	}

	/// A TSDNS server on 127.0.0.1 that answers the names in `answers` and
	/// `404` to others, or with `silent` accepts and never answers (like a
	/// server behind a firewall that drops the port). Returns its port.
	async fn fake_tsdns(answers: &'static [(&'static str, &'static str)], silent: bool) -> u16 {
		let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(async move {
			loop {
				let (mut socket, _) = listener.accept().await.unwrap();
				tokio::spawn(async move {
					let mut asked = [0; 256];
					let n = socket.read(&mut asked).await.unwrap();
					if silent {
						tokio::time::sleep(Duration::from_secs(60)).await;
					}
					let asked = str::from_utf8(&asked[..n]).unwrap();
					let answer = answers.iter().find(|a| a.0 == asked).map_or("404", |a| a.1);
					socket.write_all(answer.as_bytes()).await.unwrap();
				});
			}
		});
		port
	}

	async fn resolved(address: &str, lookups: Lookups) -> Vec<String> {
		create_logger();
		let found: Vec<SocketAddr> = tokio::time::timeout(
			Duration::from_secs(5),
			resolve_with(address.into(), lookups).try_collect(),
		)
		.await
		.expect("resolving takes too long")
		.unwrap();
		found.iter().map(ToString::to_string).collect()
	}

	/// The official client's log for such a server: `A/AAAA DNS resolve for
	/// possible TSDNS successful, "example.test"`, `TSDNS found at …:41144
	/// and queried successfully`.
	#[tokio::test]
	async fn tsdns_at_the_domain_without_srv_records() {
		let port = fake_tsdns(&[("ts.example.test", "192.0.2.10:19987")], false).await;
		let lookups =
			fake(&[("example.test", "127.0.0.1"), ("ts.example.test", "198.51.100.7")], &[], port);
		assert_eq!(resolved("ts.example.test", lookups.clone()).await, [
			"192.0.2.10:19987",
			"198.51.100.7:9987"
		]);
		// A port typed with the address wins over TSDNS and the default.
		assert_eq!(resolved("ts.example.test:4000", lookups).await, [
			"192.0.2.10:4000",
			"198.51.100.7:4000"
		]);
	}

	#[tokio::test]
	async fn tsdns_that_does_not_know_the_name_leaves_the_plain_address() {
		let port = fake_tsdns(&[("other.example.test", "192.0.2.10:19987")], false).await;
		let lookups =
			fake(&[("example.test", "127.0.0.1"), ("ts.example.test", "198.51.100.7")], &[], port);
		assert_eq!(resolved("ts.example.test", lookups).await, ["198.51.100.7:9987"]);
	}

	#[tokio::test]
	async fn a_silent_tsdns_server_does_not_hold_up_resolving() {
		let port = fake_tsdns(&[("ts.example.test", "192.0.2.10:19987")], true).await;
		let lookups =
			fake(&[("example.test", "127.0.0.1"), ("ts.example.test", "198.51.100.7")], &[], port);
		let start = std::time::Instant::now();
		assert_eq!(resolved("ts.example.test", lookups).await, ["198.51.100.7:9987"]);
		assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
	}

	/// The official order: `_ts3._udp` SRV, then TSDNS, then the address
	/// itself; and a TSDNS server named by `_tsdns._tcp` SRV is asked instead
	/// of one at the domain.
	#[tokio::test]
	async fn srv_records_come_first_and_name_the_tsdns_server() {
		let direct = fake_tsdns(&[("ts.example.test", "192.0.2.99:1")], false).await;
		let named = fake_tsdns(&[("ts.example.test", "192.0.2.20:2000")], false).await;
		let lookups = fake(
			&[
				("example.test", "127.0.0.1"),
				("tsdns.example.test", "127.0.0.1"),
				("voice.example.test", "203.0.113.5"),
				("ts.example.test", "198.51.100.7"),
			],
			&[
				("_ts3._udp.ts.example.test.", "voice.example.test", 9999),
				("_tsdns._tcp.example.test.", "tsdns.example.test", named),
			],
			direct,
		);
		assert_eq!(resolved("ts.example.test", lookups).await, [
			"203.0.113.5:9999",
			"192.0.2.20:2000",
			"198.51.100.7:9987"
		]);
	}

	#[test]
	fn tsdns_is_looked_for_at_the_parent_domains() {
		assert_eq!(tsdns_domains("ts.example.test"), ["example.test"]);
		assert_eq!(tsdns_domains("a.b.example.test"), ["b.example.test", "example.test"]);
		assert_eq!(tsdns_domains("example.test"), ["example.test"]);
		assert!(tsdns_domains("localhost").is_empty());
	}

	#[test]
	fn srv_records_are_ordered_by_priority_and_none_is_lost() {
		// Weight 0 is the usual weight; such records were dropped before.
		assert_eq!(order_srv(vec![(0, 0, "only")]), ["only"]);
		for _ in 0..50 {
			let ordered = order_srv(vec![(10, 0, "c"), (0, 0, "a"), (0, 5, "b"), (20, 1, "d")]);
			assert_eq!(ordered.len(), 4);
			assert!(ordered[..2].contains(&"a") && ordered[..2].contains(&"b"), "{:?}", ordered);
			assert_eq!(ordered[2..], ["c", "d"]);
		}
	}
}
