//! Pictures from the web: the host banner and TeamSpeak 6 channel banners,
//! which servers give as `http(s)` addresses on any host.
//!
//! They go into the engine's cache ([`crate::cache`], named by the MD5 hash
//! of the address) like avatars and icons, and only with
//! `cache.fetch_images`: fetching one contacts a host the server chose, as
//! the official client does. A picture is at most
//! [`cache::MAX_PICTURE_BYTES`] and goes to disk as it arrives; it connects
//! within [`CONNECT_TIMEOUT`], and a download fails when nothing arrives for
//! [`READ_TIMEOUT`] or, after [`GRACE`], when it averages less than
//! [`cache::MIN_PICTURE_RATE`]: a large banner on a slow host still arrives.
//! It is kept only if its content is a picture the UI shows (PNG, JPEG,
//! GIF, WebP, SVG), whatever its address or the server say. The client is
//! the workspace's reqwest with rustls: system proxies (`HTTPS_PROXY`,
//! `NO_PROXY`, …) and the platform's certificate store.
//!
//! Banners in the server's own files (`ts3image://`) come through the voice
//! connection instead ([`crate::files::server_image`]).

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::cache::{self, Cache, Fetch, Waiter};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a download may start slowly before its rate counts.
const GRACE: Duration = Duration::from_secs(30);
/// The first bytes, enough to tell a picture from an error page.
const SNIFF: usize = 1024;
/// The pictures the UI decodes, so a host choosing between formats
/// (`Accept`) does not answer with one it cannot show (AVIF, JPEG XL).
const ACCEPT: &str = "image/png,image/jpeg,image/gif,image/webp,image/svg+xml;q=0.9,*/*;q=0.1";
/// Fetch independent URLs in parallel without letting a large channel tree
/// open unbounded connections or buffer unbounded image data.
static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
/// The host banner is reloaded at most this often, whatever the server
/// asks (TeamSpeak 3 and 6 servers refuse intervals below a minute).
pub(crate) const MIN_RELOAD: Duration = Duration::from_secs(60);

static CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
	reqwest::Client::builder()
		.connect_timeout(CONNECT_TIMEOUT)
		.read_timeout(READ_TIMEOUT)
		.user_agent(concat!("Voelin/", env!("CARGO_PKG_VERSION")))
		.build()
		.map_err(|e| e.to_string())
});

/// Get the picture at `url` into `cache`, downloading it once however many
/// ask (again with `fresh`: it changes at its address); `waiter` is told
/// where it is. `max_cache_bytes`: `cache.max_mb` in bytes.
pub(crate) fn fetch(cache: Cache, url: &str, fresh: bool, max_cache_bytes: u64, waiter: Waiter) {
	let Some(key) = cache::picture_key(url) else {
		waiter(Err("not an http or https address".into()));
		return;
	};
	let Fetch::Download(temp) = cache.fetch(&key, fresh, waiter) else { return };
	let url = url.to_owned();
	tokio::spawn(async move {
		let _permit = DOWNLOADS.acquire().await.expect("download semaphore stays open");
		let result = download(&url, &temp, &LIMITS).await;
		cache.finish(&key, &temp, result, max_cache_bytes);
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

/// Download the picture at `url` into `to` within `limits`, and only if it
/// is a picture (what arrived stays in `to` on failure; the cache removes it).
async fn download(url: &str, to: &Path, limits: &Limits) -> Result<(), String> {
	let client = CLIENT.as_ref().map_err(Clone::clone)?;
	let too_big = || format!("larger than {} KiB", limits.max_bytes >> 10);
	let mut response = client
		.get(url)
		.header(reqwest::header::ACCEPT, ACCEPT)
		.send()
		.await
		.and_then(reqwest::Response::error_for_status)
		.map_err(|e| e.to_string())?;
	if response.content_length().is_some_and(|n| n > limits.max_bytes) {
		return Err(too_big());
	}
	if let Some(dir) = to.parent() {
		tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
	}
	let mut file = tokio::fs::File::create(to).await.map_err(|e| e.to_string())?;
	let started = tokio::time::Instant::now();
	let mut head = Vec::with_capacity(SNIFF);
	let mut received = 0u64;
	while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
		received += chunk.len() as u64;
		if received > limits.max_bytes {
			return Err(too_big());
		}
		if head.len() < SNIFF {
			head.extend_from_slice(&chunk[..chunk.len().min(SNIFF - head.len())]);
			// An error page is refused without waiting for all of it.
			if head.len() == SNIFF && !is_picture(&head) {
				return Err("not a picture".into());
			}
		}
		let elapsed = started.elapsed();
		let expected = limits.min_rate.saturating_mul(elapsed.as_millis() as u64) / 1000;
		if elapsed > limits.grace && received < expected {
			return Err(format!("too slow: {} KiB in {} s", received >> 10, elapsed.as_secs()));
		}
		file.write_all(&chunk).await.map_err(|e| e.to_string())?;
	}
	file.flush().await.map_err(|e| e.to_string())?;
	drop(file);
	if !is_picture(&head) {
		return Err("not a picture".into());
	}
	Ok(())
}

/// Whether `data` is a picture the UI decodes, told by its content as the
/// UI tells it (SVG by its start, without leading blanks).
fn is_picture(data: &[u8]) -> bool {
	const STARTS: [&[u8]; 4] = [b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff", b"GIF87a", b"GIF89a"];
	let xml = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data).trim_ascii_start();
	STARTS.iter().any(|s| data.starts_with(s))
		|| xml.starts_with(b"<?xml")
		|| xml.starts_with(b"<svg")
		|| (data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP")
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc;

	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	use tokio::net::TcpListener;

	use super::*;

	const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\xdac\xf8\xcf\xc0\xf0\x1f\0\x05\0\x01\xff\x89\x99=\x1d\0\0\0\0IEND\xaeB`\x82";

	/// An HTTP server on 127.0.0.1 with a few answers; returns its address.
	async fn server() -> String {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		tokio::spawn(async move {
			loop {
				let Ok((mut socket, _)) = listener.accept().await else { return };
				tokio::spawn(async move {
					let mut request = Vec::new();
					let mut buf = [0u8; 1024];
					while !request.windows(4).any(|w| w == b"\r\n\r\n") {
						match socket.read(&mut buf).await {
							Ok(0) | Err(_) => return,
							Ok(n) => request.extend_from_slice(&buf[..n]),
						}
					}
					let path = String::from_utf8_lossy(&request)
						.split_whitespace()
						.nth(1)
						.unwrap_or("/")
						.to_owned();
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
						"/long-page" => {
							("200 OK", false, [b"<!DOCTYPE html>", &[b' '; 4000][..]].concat())
						}
						// A picture, then a byte every 100 ms.
						"/slow" => ("200 OK", true, [PNG, &[0; 20]].concat()),
						_ => ("404 Not Found", true, b"no".to_vec()),
					};
					let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
					if length {
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
				});
			}
		});
		format!("http://{addr}")
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
		download(&format!("{base}/banner"), &to, &limits).await.unwrap();
		assert_eq!(std::fs::read(&to).unwrap(), PNG);
		download(&format!("{base}/svg"), &to, &limits).await.unwrap();
		for (path, error) in [
			("/big", "larger than"),
			("/big-unannounced", "larger than"),
			("/page", "not a picture"),
			("/missing", "404"),
		] {
			let e = download(&format!("{base}{path}"), &to, &limits).await.unwrap_err();
			assert!(e.contains(error), "{path}: {e}");
		}
		// An error page longer than the first bytes looked at, unannounced.
		let e = download(&format!("{base}/long-page"), &to, &LIMITS).await.unwrap_err();
		assert!(e.contains("not a picture"), "{e}");
		// Nothing listens there.
		let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
		assert!(download(&format!("http://127.0.0.1:{port}/b"), &to, &limits).await.is_err());
		std::fs::remove_dir_all(dir).unwrap();
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
		assert!(rx.recv().unwrap().unwrap_err().contains("http"));
		// A failed download is reported and leaves nothing behind.
		fetch(cache.clone(), &format!("{base}/page"), false, 0, waiter(&tx));
		let failed = tokio::task::spawn_blocking(move || rx.recv().unwrap()).await.unwrap();
		assert_eq!(failed, Err("not a picture".into()));
		assert_eq!(cache.size(), PNG.len() as u64);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn banners_above_the_old_limits_are_downloaded() {
		let base = server().await;
		let dir = temp_dir("large-banner");
		let path = dir.join("large");
		download(&format!("{base}/large-banner"), &path, &LIMITS).await.unwrap();
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
		download(&format!("{base}/slow"), &to, &patient).await.unwrap();
		let strict = Limits { grace: Duration::from_millis(150), min_rate: 1 << 20, ..LIMITS };
		let e = download(&format!("{base}/slow"), &to, &strict).await.unwrap_err();
		assert!(e.contains("too slow"), "{e}");
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn distinct_banners_download_concurrently_and_duplicates_share_a_request() {
		use std::sync::Arc;
		use tokio::sync::{Semaphore, mpsc as async_mpsc};
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		let (requests, mut received) = async_mpsc::unbounded_channel();
		let release = Arc::new(Semaphore::new(0));
		let gate = release.clone();
		let server = tokio::spawn(async move {
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
			tokio::time::timeout(Duration::from_secs(5), received.recv()).await.unwrap().unwrap();
		}
		release.add_permits(3);
		for _ in 0..4 {
			let path = tokio::time::timeout(Duration::from_secs(5), results.recv())
				.await
				.unwrap()
				.unwrap()
				.unwrap();
			assert_eq!(std::fs::read(path).unwrap(), PNG);
		}
		assert!(received.try_recv().is_err(), "duplicate URL opened another connection");
		server.abort();
		std::fs::remove_dir_all(dir).unwrap();
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
	}
}
