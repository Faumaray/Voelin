//! Pictures from the web: the host banner and TeamSpeak 6 channel banners,
//! which servers give as `http(s)` addresses on any host.
//!
//! They go into the engine's cache ([`crate::cache`], named by the MD5 hash
//! of the address) like avatars and icons, and only with
//! `cache.fetch_images`: fetching one contacts a host the server chose, as
//! the official client does. A picture is at most [`MAX_BYTES`], arrives
//! within [`TIMEOUT`] (connected within [`CONNECT_TIMEOUT`]) and is kept
//! only if its content is a picture the UI shows (PNG, JPEG, GIF, WebP,
//! SVG), whatever its address or the server say. The client is the
//! workspace's reqwest with rustls: system proxies (`HTTPS_PROXY`,
//! `NO_PROXY`, …) and the platform's certificate store.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use crate::cache::{self, Cache, Fetch, Waiter};

/// Larger pictures are refused; banners are rarely above a few hundred KiB.
pub(crate) const MAX_BYTES: u64 = 4 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(30);
/// The host banner is reloaded at most this often, whatever the server
/// asks (TeamSpeak 3 and 6 servers refuse intervals below a minute).
pub(crate) const MIN_RELOAD: Duration = Duration::from_secs(60);

static CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
	reqwest::Client::builder()
		.connect_timeout(CONNECT_TIMEOUT)
		.timeout(TIMEOUT)
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
		let result = download(&url, &temp, MAX_BYTES).await;
		cache.finish(&key, &temp, result, max_cache_bytes);
	});
}

/// Download the picture at `url` into `to`: at most `max_bytes`, and only
/// if it is a picture.
async fn download(url: &str, to: &Path, max_bytes: u64) -> Result<(), String> {
	let client = CLIENT.as_ref().map_err(Clone::clone)?;
	let too_big = || format!("larger than {} KiB", max_bytes >> 10);
	let mut response = client
		.get(url)
		.send()
		.await
		.and_then(reqwest::Response::error_for_status)
		.map_err(|e| e.to_string())?;
	if response.content_length().is_some_and(|n| n > max_bytes) {
		return Err(too_big());
	}
	let mut data = Vec::new();
	while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
		if (data.len() + chunk.len()) as u64 > max_bytes {
			return Err(too_big());
		}
		data.extend_from_slice(&chunk);
	}
	if !is_picture(&data) {
		return Err("not a picture".into());
	}
	if let Some(dir) = to.parent() {
		tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
	}
	tokio::fs::write(to, &data).await.map_err(|e| e.to_string())
}

/// Whether `data` is a picture the UI decodes, told by its content as the
/// UI tells it (SVG by its start, without leading blanks).
fn is_picture(data: &[u8]) -> bool {
	const STARTS: [&[u8]; 6] =
		[b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff", b"GIF87a", b"GIF89a", b"<?xml", b"<svg"];
	STARTS.iter().any(|s| data.starts_with(s))
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
						// Too big, as announced or as it comes.
						"/big" => ("200 OK", true, [PNG, &[0; 4000]].concat()),
						"/big-unannounced" => ("200 OK", false, [PNG, &[0; 4000]].concat()),
						"/page" => ("200 OK", true, b"<!DOCTYPE html><html></html>".to_vec()),
						_ => ("404 Not Found", true, b"no".to_vec()),
					};
					let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
					if length {
						head += &format!("Content-Length: {}\r\n", body.len());
					}
					head += "\r\n";
					let _ = socket.write_all(head.as_bytes()).await;
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
		download(&format!("{base}/banner"), &to, 1000).await.unwrap();
		assert_eq!(std::fs::read(&to).unwrap(), PNG);
		download(&format!("{base}/svg"), &to, 1000).await.unwrap();
		for (path, error) in [
			("/big", "larger than"),
			("/big-unannounced", "larger than"),
			("/page", "not a picture"),
			("/missing", "404"),
		] {
			let e = download(&format!("{base}{path}"), &to, 1000).await.unwrap_err();
			assert!(e.contains(error), "{path}: {e}");
		}
		// Nothing listens there.
		let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
		assert!(download(&format!("http://127.0.0.1:{port}/b"), &to, 1000).await.is_err());
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

	#[test]
	fn pictures_by_content() {
		assert!(is_picture(PNG));
		assert!(is_picture(b"\xff\xd8\xff\xe0\0\x10JFIF"));
		assert!(is_picture(b"GIF89a\x01\0\x01\0"));
		assert!(is_picture(b"RIFF\x24\0\0\0WEBPVP8 "));
		assert!(is_picture(b"<?xml version=\"1.0\"?><svg/>"));
		assert!(!is_picture(b"<!DOCTYPE html>"));
		assert!(!is_picture(b"RIFF\x24\0\0\0WAVEfmt "));
		assert!(!is_picture(b""));
	}
}
