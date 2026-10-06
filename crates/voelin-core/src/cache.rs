//! The avatar and icon cache: files on disk named by their content.
//!
//! An avatar's name is its MD5 hash (`client_flag_avatar`), an icon's its
//! id (the CRC32 of the image, as TeamSpeak assigns them), so a lookup is a
//! map access, and the same image seen on several servers or under
//! several clients is one file. Pictures from the web (banners,
//! [`crate::web`]) are named by the MD5 hash of their address. Keys are
//! relative paths (`avatars/<md5>`, `icons/<id>`, `pictures/<md5>`).
//!
//! The cache is bounded by `cache.max_mb` (read on every insert; 0: no
//! limit): the least recently used files go first. Use times survive
//! restarts as the files' modification times.
//!
//! Downloads are deduplicated: [`Cache::fetch`] tells the first asker to
//! download (into a temporary path), later askers wait for the same
//! download ([`Cache::finish`] answers them all).
//!
//! A picture from the web keeps its host's validators (`ETag`,
//! `Last-Modified`) next to it, in `validators/pictures/<md5>`: asked for
//! again, it is downloaded only if it changed ([`Cache::validators`],
//! [`Arrived::Unchanged`]).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use tracing::debug;

/// Called with the cached file's path, or why it could not be fetched.
pub(crate) type Waiter = Box<dyn FnOnce(Result<PathBuf, FetchError>) + Send>;

/// Why a fetch failed (for the log), and when it may be tried again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FetchError {
	pub text: String,
	pub retry: RetryHint,
}

/// When a failed fetch may be tried again, as its host told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RetryHint {
	/// Whenever the asker's own schedule says.
	#[default]
	Default,
	/// Not before this long (the host's `Retry-After`).
	After(Duration),
	/// The address is wrong or gone (HTTP 400, 404, 410): not soon.
	Permanent,
}

impl FetchError {
	pub fn new(text: impl Into<String>, retry: RetryHint) -> Self {
		Self { text: text.into(), retry }
	}
}

impl From<String> for FetchError {
	fn from(text: String) -> Self {
		Self::new(text, RetryHint::Default)
	}
}

impl From<&str> for FetchError {
	fn from(text: &str) -> Self {
		Self::new(text, RetryHint::Default)
	}
}

impl fmt::Display for FetchError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.text)
	}
}

/// What a host said identifies the version of a picture it sent, to ask
/// later whether it changed (`If-None-Match`, `If-Modified-Since`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Validators {
	pub etag: Option<String>,
	pub last_modified: Option<String>,
}

impl Validators {
	pub fn is_empty(&self) -> bool {
		self.etag.is_none() && self.last_modified.is_none()
	}

	/// As kept: a `name: value` line each.
	fn to_text(&self) -> String {
		let mut text = String::new();
		for (name, value) in [("etag", &self.etag), ("last-modified", &self.last_modified)] {
			if let Some(value) = value {
				text.push_str(&format!("{name}: {value}\n"));
			}
		}
		text
	}

	fn from_text(text: &str) -> Self {
		let mut validators = Self::default();
		for line in text.lines() {
			match line.split_once(": ") {
				Some(("etag", value)) => validators.etag = Some(value.to_owned()),
				Some(("last-modified", value)) => validators.last_modified = Some(value.to_owned()),
				_ => {}
			}
		}
		validators
	}
}

/// How a download ended well.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Arrived {
	/// In the temporary path, with what identifies its version (when the
	/// host said).
	File(Validators),
	/// The host said the cached copy is still what it has (HTTP 304): the
	/// copy stays, nothing was downloaded.
	Unchanged,
}

/// What [`Cache::fetch`] decided.
pub(crate) enum Fetch {
	/// In the cache; the waiter was answered.
	Cached,
	/// Another download is running; the waiter is answered when it ends.
	Waiting,
	/// Download into this path, then call [`Cache::finish`].
	Download(PathBuf),
}

struct Entry {
	size: u64,
	tick: u64,
}

#[derive(Default)]
struct Index {
	scanned: bool,
	entries: HashMap<String, Entry>,
	/// Use order: oldest first.
	lru: BTreeMap<u64, String>,
	total: u64,
	next_tick: u64,
	in_flight: HashMap<String, Vec<Waiter>>,
	temp_serial: u64,
}

/// See the [module docs](self). Cheap to clone.
#[derive(Clone)]
pub struct Cache {
	dir: Arc<PathBuf>,
	index: Arc<Mutex<Index>>,
}

impl std::fmt::Debug for Cache {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Cache").field("dir", &self.dir).finish()
	}
}

/// The cache key of an avatar (its MD5 hash, hex).
pub(crate) fn avatar_key(hash: &str) -> Option<String> {
	let valid =
		!hash.is_empty() && hash.len() <= 128 && hash.bytes().all(|b| b.is_ascii_hexdigit());
	valid.then(|| format!("avatars/{}", hash.to_ascii_lowercase()))
}

/// The cache key of an icon.
pub(crate) fn icon_key(id: u32) -> String {
	format!("icons/{id}")
}

/// The largest picture downloaded into the cache, from the web or from the
/// server's files: animated banners of several megabytes are common. The
/// download goes to disk as it arrives, so this bounds the disk, not memory.
pub(crate) const MAX_PICTURE_BYTES: u64 = 128 << 20;

/// The slowest a picture may arrive on average after its first seconds:
/// a large banner on a slow link still arrives, one trickling in forever
/// does not hold a download slot.
pub(crate) const MIN_PICTURE_RATE: u64 = 8 << 10;

/// The cache key of a picture on the web: only `http` and `https`
/// addresses (blanks around one are not part of it).
pub(crate) fn picture_key(url: &str) -> Option<String> {
	let url = url.trim();
	let (scheme, rest) = url.split_once("://")?;
	let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
	(web && !rest.is_empty()).then(|| format!("pictures/{:x}", md5::compute(url)))
}

/// The cache key of a picture in a server's files (`ts3image://`): the
/// same address names another file on another server.
pub(crate) fn server_picture_key(server: &str, url: &str) -> String {
	format!("pictures/{:x}", md5::compute(format!("{server}\n{url}")))
}

impl Cache {
	/// A cache in `dir` (created when the first file arrives).
	pub fn new(dir: impl Into<PathBuf>) -> Self {
		Self { dir: Arc::new(dir.into()), index: Arc::default() }
	}

	/// The platform's cache directory (`<cache>/voelin/images`).
	pub fn default_dir() -> PathBuf {
		voelin_platform::paths::cache_dir().join("images")
	}

	pub fn dir(&self) -> &Path {
		&self.dir
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
		let mut index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
		if !index.scanned {
			index.scanned = true;
			scan(&self.dir, &mut index);
		}
		index
	}

	/// The file of `key` if it is cached (marked as used).
	pub fn get(&self, key: &str) -> Option<PathBuf> {
		let mut index = self.lock();
		self.touch(&mut index, key)
	}

	/// The cached server, channel, group or client icon, if available. Not
	/// marked as used (no file is opened or touched): the UI asks on every
	/// refresh of the server list.
	pub fn icon(&self, id: u32) -> Option<PathBuf> {
		if id == 0 {
			return None;
		}
		let key = icon_key(id);
		let path = self.dir.join(&key);
		(self.lock().entries.contains_key(&key) && path.is_file()).then_some(path)
	}

	fn touch(&self, index: &mut Index, key: &str) -> Option<PathBuf> {
		let path = self.dir.join(key);
		let tick = index.entries.get(key)?.tick;
		if !path.is_file() {
			// Deleted behind our back.
			let entry = index.entries.remove(key)?;
			index.lru.remove(&entry.tick);
			index.total = index.total.saturating_sub(entry.size);
			return None;
		}
		index.lru.remove(&tick);
		let tick = index.next_tick;
		index.next_tick += 1;
		index.lru.insert(tick, key.to_owned());
		if let Some(e) = index.entries.get_mut(key) {
			e.tick = tick;
		}
		// The use time for the next run (best effort).
		if let Ok(file) = std::fs::File::options().append(true).open(&path) {
			let _ = file.set_modified(SystemTime::now());
		}
		Some(path)
	}

	/// Get `key`, downloading it once however many ask (see [`Fetch`]).
	/// With `fresh`, download it again even if it is cached (a picture that
	/// changes at its address); the new file replaces the old one.
	pub(crate) fn fetch(&self, key: &str, fresh: bool, waiter: Waiter) -> Fetch {
		let mut index = self.lock();
		if !fresh && let Some(path) = self.touch(&mut index, key) {
			drop(index);
			waiter(Ok(path));
			return Fetch::Cached;
		}
		if let Some(waiters) = index.in_flight.get_mut(key) {
			waiters.push(waiter);
			return Fetch::Waiting;
		}
		index.in_flight.insert(key.to_owned(), vec![waiter]);
		index.temp_serial += 1;
		let temp = self.dir.join("tmp").join(format!(
			"{}-{}-{}.part",
			key.replace('/', "_"),
			std::process::id(),
			index.temp_serial
		));
		Fetch::Download(temp)
	}

	/// The download of `key` into `temp` ended; keep at most `max_bytes`
	/// (0: no limit) and answer everyone waiting.
	pub(crate) fn finish(
		&self,
		key: &str,
		temp: &Path,
		result: Result<(), FetchError>,
		max_bytes: u64,
	) {
		let arrived = result.map(|()| Arrived::File(Validators::default()));
		self.finish_with(key, temp, arrived, max_bytes);
	}

	/// [`finish`](Self::finish) a download from the web: a new file with
	/// its validators, or the cached copy confirmed (kept, and marked as
	/// used).
	pub(crate) fn finish_with(
		&self,
		key: &str,
		temp: &Path,
		result: Result<Arrived, FetchError>,
		max_bytes: u64,
	) {
		let result = result.and_then(|arrived| match arrived {
			Arrived::File(validators) => {
				let path = self.insert_file(key, temp, max_bytes)?;
				self.keep_validators(key, &validators);
				Ok(path)
			}
			Arrived::Unchanged => {
				let mut index = self.lock();
				self.touch(&mut index, key)
					.ok_or_else(|| FetchError::from("cache: the copy was removed meanwhile"))
			}
		});
		let _ = std::fs::remove_file(temp);
		let waiters = self.lock().in_flight.remove(key).unwrap_or_default();
		for waiter in waiters {
			waiter(result.clone());
		}
	}

	/// What identifies the cached version of `key` (a picture from the
	/// web), if it is cached and its host said.
	pub(crate) fn validators(&self, key: &str) -> Option<Validators> {
		if !self.lock().entries.contains_key(key) || !self.dir.join(key).is_file() {
			return None;
		}
		let text = std::fs::read_to_string(validators_path(&self.dir, key)).ok()?;
		Some(Validators::from_text(&text)).filter(|v| !v.is_empty())
	}

	/// Keep `validators` with `key`'s new file (none: the old ones go).
	fn keep_validators(&self, key: &str, validators: &Validators) {
		let path = validators_path(&self.dir, key);
		if validators.is_empty() {
			let _ = std::fs::remove_file(&path);
			return;
		}
		let written = path
			.parent()
			.map_or(Ok(()), std::fs::create_dir_all)
			.and_then(|()| std::fs::write(&path, validators.to_text()));
		if let Err(e) = written {
			debug!("cache: cannot keep the validators of {key}: {e}");
		}
	}

	/// Put a file we have (e.g. our own avatar) into the cache.
	pub(crate) fn insert_copy(
		&self,
		key: &str,
		from: &Path,
		max_bytes: u64,
	) -> Result<PathBuf, String> {
		let temp = self.dir.join("tmp").join(format!("{}.copy", key.replace('/', "_")));
		std::fs::create_dir_all(temp.parent().expect("tmp dir")).map_err(|e| e.to_string())?;
		std::fs::copy(from, &temp).map_err(|e| e.to_string())?;
		let result = self.insert_file(key, &temp, max_bytes);
		let _ = std::fs::remove_file(&temp);
		result
	}

	fn insert_file(&self, key: &str, temp: &Path, max_bytes: u64) -> Result<PathBuf, String> {
		let path = self.dir.join(key);
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir).map_err(|e| format!("cache: {e}"))?;
		}
		let size = std::fs::metadata(temp).map_err(|e| format!("cache: {e}"))?.len();
		std::fs::rename(temp, &path).map_err(|e| format!("cache: {e}"))?;
		let mut index = self.lock();
		if let Some(old) = index.entries.remove(key) {
			index.lru.remove(&old.tick);
			index.total = index.total.saturating_sub(old.size);
		}
		let tick = index.next_tick;
		index.next_tick += 1;
		index.entries.insert(key.to_owned(), Entry { size, tick });
		index.lru.insert(tick, key.to_owned());
		index.total += size;
		self.evict(&mut index, max_bytes, key);
		Ok(path)
	}

	/// Remove least recently used files until the cache fits `max_bytes`
	/// (never `keep`, the file just added).
	fn evict(&self, index: &mut Index, max_bytes: u64, keep: &str) {
		if max_bytes == 0 {
			return;
		}
		while index.total > max_bytes {
			let Some((&tick, key)) = index.lru.iter().find(|(_, k)| k.as_str() != keep) else {
				break;
			};
			let key = key.clone();
			index.lru.remove(&tick);
			if let Some(entry) = index.entries.remove(&key) {
				index.total = index.total.saturating_sub(entry.size);
			}
			if let Err(e) = std::fs::remove_file(self.dir.join(&key)) {
				debug!("cache: cannot remove {key}: {e}");
			}
			let _ = std::fs::remove_file(validators_path(&self.dir, &key));
		}
	}

	/// Bytes in the cache.
	pub fn size(&self) -> u64 {
		self.lock().total
	}
}

/// Index what is on disk, oldest use first; leftover temporary files go.
fn scan(dir: &Path, index: &mut Index) {
	let _ = std::fs::remove_dir_all(dir.join("tmp"));
	let mut found = Vec::new();
	for sub in ["avatars", "icons", "pictures"] {
		let Ok(entries) = std::fs::read_dir(dir.join(sub)) else { continue };
		for entry in entries.flatten() {
			let Ok(meta) = entry.metadata() else { continue };
			if !meta.is_file() {
				continue;
			}
			let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
			let key = format!("{sub}/{}", entry.file_name().to_string_lossy());
			found.push((used, key, meta.len()));
		}
	}
	found.sort();
	for (_, key, size) in found {
		let tick = index.next_tick;
		index.next_tick += 1;
		index.total += size;
		index.lru.insert(tick, key.clone());
		index.entries.insert(key, Entry { size, tick });
	}
	// Validators of pictures no longer here.
	let Ok(kept) = std::fs::read_dir(dir.join(VALIDATORS).join("pictures")) else { return };
	for entry in kept.flatten() {
		let key = format!("pictures/{}", entry.file_name().to_string_lossy());
		if !index.entries.contains_key(&key) {
			let _ = std::fs::remove_file(entry.path());
		}
	}
}

/// The directory of the pictures' validators.
const VALIDATORS: &str = "validators";

/// Where the validators of `key` are kept.
fn validators_path(dir: &Path, key: &str) -> PathBuf {
	dir.join(VALIDATORS).join(key)
}

/// The engine's cache: one [`Cache`] that [`crate::Command::AttachCache`]
/// can replace, shared by the sessions.
#[derive(Clone, Debug)]
pub(crate) struct SharedCache(Arc<RwLock<Cache>>);

impl SharedCache {
	pub fn new(cache: Cache) -> Self {
		Self(Arc::new(RwLock::new(cache)))
	}

	pub fn current(&self) -> Cache {
		self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
	}

	pub fn replace(&self, cache: Cache) {
		*self.0.write().unwrap_or_else(PoisonError::into_inner) = cache;
	}
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc;

	use super::*;

	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"voelin-cache-{tag}-{}-{:?}",
			std::process::id(),
			std::thread::current().id()
		));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	/// Fetch `key` and write `bytes` as its download.
	fn put(cache: &Cache, key: &str, bytes: usize, max: u64) -> PathBuf {
		let (tx, rx) = mpsc::channel();
		let Fetch::Download(temp) = cache.fetch(key, false, Box::new(move |r| tx.send(r).unwrap()))
		else {
			panic!("{key} should download");
		};
		std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
		std::fs::write(&temp, vec![0u8; bytes]).unwrap();
		cache.finish(key, &temp, Ok(()), max);
		rx.recv().unwrap().unwrap()
	}

	#[test]
	fn fetch_once_then_hit() {
		let dir = temp_dir("fetch");
		let cache = Cache::new(&dir);
		let (tx, rx) = mpsc::channel();
		let waiter = |tx: mpsc::Sender<_>| -> Waiter { Box::new(move |r| tx.send(r).unwrap()) };
		let key = avatar_key("ABCDEF0123").unwrap();
		assert_eq!(key, "avatars/abcdef0123");
		let Fetch::Download(temp) = cache.fetch(&key, false, waiter(tx.clone())) else { panic!() };
		// A second asker waits for the same download.
		assert!(matches!(cache.fetch(&key, false, waiter(tx.clone())), Fetch::Waiting));
		std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
		std::fs::write(&temp, b"png").unwrap();
		cache.finish(&key, &temp, Ok(()), 0);
		let a = rx.recv().unwrap().unwrap();
		let b = rx.recv().unwrap().unwrap();
		assert_eq!(a, b);
		assert_eq!(std::fs::read(&a).unwrap(), b"png");
		assert!(!temp.exists());
		assert!(matches!(cache.fetch(&key, false, waiter(tx.clone())), Fetch::Cached));
		assert_eq!(rx.recv().unwrap().unwrap(), a);
		assert_eq!(cache.get(&key), Some(a.clone()));
		assert_eq!(cache.size(), 3);
		// Fresh: downloaded again, the new file replaces the old one.
		let Fetch::Download(temp) = cache.fetch(&key, true, waiter(tx)) else { panic!() };
		std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
		std::fs::write(&temp, b"newer").unwrap();
		cache.finish(&key, &temp, Ok(()), 0);
		assert_eq!(rx.recv().unwrap().unwrap(), a);
		assert_eq!(std::fs::read(&a).unwrap(), b"newer");
		assert_eq!(cache.size(), 5);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn failed_download_answers_everyone_and_retries() {
		let dir = temp_dir("fail");
		let cache = Cache::new(&dir);
		let (tx, rx) = mpsc::channel();
		let tx2 = tx.clone();
		let Fetch::Download(temp) =
			cache.fetch("icons/7", false, Box::new(move |r| tx.send(r).unwrap()))
		else {
			panic!()
		};
		assert!(matches!(
			cache.fetch("icons/7", false, Box::new(move |r| tx2.send(r).unwrap())),
			Fetch::Waiting
		));
		cache.finish("icons/7", &temp, Err("file not found".into()), 0);
		assert_eq!(rx.recv().unwrap(), Err("file not found".into()));
		assert_eq!(rx.recv().unwrap(), Err("file not found".into()));
		assert!(cache.get("icons/7").is_none());
		assert!(matches!(cache.fetch("icons/7", false, Box::new(|_| {})), Fetch::Download(_)));
	}

	#[test]
	fn failed_picture_refresh_keeps_previous_file_and_can_retry() {
		let dir = temp_dir("refresh-fail");
		let cache = Cache::new(&dir);
		let key = picture_key("https://example.com/banner.png").unwrap();
		let path = put(&cache, &key, 32, 0);
		let (tx, rx) = mpsc::channel();
		let Fetch::Download(temp) = cache.fetch(&key, true, Box::new(move |r| tx.send(r).unwrap()))
		else {
			panic!("refresh download");
		};
		std::fs::write(&temp, b"partial download").unwrap();
		cache.finish(&key, &temp, Err("connection lost".into()), 0);
		assert_eq!(rx.recv().unwrap(), Err("connection lost".into()));
		assert!(!temp.exists());
		assert_eq!(cache.get(&key), Some(path.clone()));
		assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 32]);
		assert_eq!(cache.size(), 32);
		assert!(matches!(cache.fetch(&key, true, Box::new(|_| {})), Fetch::Download(_)));
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn evicts_least_recently_used_and_survives_restart() {
		let dir = temp_dir("lru");
		let cache = Cache::new(&dir);
		let a = put(&cache, "icons/1", 400, 1000);
		let b = put(&cache, "icons/2", 400, 1000);
		// `a` is used again: `b` is now the oldest.
		assert_eq!(cache.get("icons/1"), Some(a.clone()));
		let c = put(&cache, "icons/3", 400, 1000);
		assert!(a.exists() && c.exists());
		assert!(!b.exists(), "the least recently used file goes");
		assert_eq!(cache.size(), 800);
		// No limit: nothing goes.
		put(&cache, "icons/4", 5000, 0);
		assert_eq!(cache.size(), 5800);
		// A new instance finds the files.
		let again = Cache::new(&dir);
		assert_eq!(again.size(), 5800);
		assert_eq!(again.get("icons/3"), Some(c));
		// A file bigger than the limit stays until the next one comes.
		put(&again, "icons/5", 3000, 1000);
		assert_eq!(again.size(), 3000);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn keys() {
		assert!(avatar_key("").is_none());
		assert!(avatar_key("../x").is_none());
		assert_eq!(icon_key(4_294_967_295), "icons/4294967295");
		let url = "https://example.com/banner.png";
		assert_eq!(picture_key(url).unwrap(), format!("pictures/{:x}", md5::compute(url)));
		assert_ne!(picture_key(url), picture_key("http://example.com/banner.png"));
		assert!(picture_key("HTTP://example.com/b.gif").is_some());
		for not_web in ["", "file:///etc/passwd", "ftp://h/b.png", "https://", "example.com/b.png"]
		{
			assert!(picture_key(not_web).is_none(), "{not_web}");
		}
	}

	/// A picture keeps its host's validators; confirmed unchanged, the copy
	/// stays and is used; replaced without validators or evicted, they go.
	#[test]
	fn validators_go_with_their_picture() {
		let dir = temp_dir("validators");
		let cache = Cache::new(&dir);
		let key = picture_key("https://example.com/v.png").unwrap();
		let validators = Validators {
			etag: Some("W/\"v1\"".into()),
			last_modified: Some("Tue, 06 Oct 2026 10:00:00 GMT".into()),
		};
		assert_eq!(cache.validators(&key), None);
		let (tx, rx) = mpsc::channel();
		let waiter = |tx: &mpsc::Sender<_>| -> Waiter {
			let tx = tx.clone();
			Box::new(move |r| tx.send(r).unwrap())
		};
		let Fetch::Download(temp) = cache.fetch(&key, false, waiter(&tx)) else { panic!() };
		std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
		std::fs::write(&temp, b"v1").unwrap();
		cache.finish_with(&key, &temp, Ok(Arrived::File(validators.clone())), 0);
		let path = rx.recv().unwrap().unwrap();
		assert_eq!(cache.validators(&key), Some(validators.clone()));
		// Asked again: unchanged, the same file answers both askers.
		let Fetch::Download(temp) = cache.fetch(&key, true, waiter(&tx)) else { panic!() };
		assert!(matches!(cache.fetch(&key, true, waiter(&tx)), Fetch::Waiting));
		cache.finish_with(&key, &temp, Ok(Arrived::Unchanged), 0);
		assert_eq!(rx.recv().unwrap(), Ok(path.clone()));
		assert_eq!(rx.recv().unwrap(), Ok(path.clone()));
		assert_eq!(std::fs::read(&path).unwrap(), b"v1");
		// They survive a restart, with their picture.
		assert_eq!(Cache::new(&dir).validators(&key), Some(validators.clone()));
		// A new version without validators: the old ones are not sent again.
		let Fetch::Download(temp) = cache.fetch(&key, true, waiter(&tx)) else { panic!() };
		// (The restart above cleared the temporary files' directory.)
		std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
		std::fs::write(&temp, b"new").unwrap();
		cache.finish(&key, &temp, Ok(()), 0);
		rx.recv().unwrap().unwrap();
		assert_eq!(cache.validators(&key), None);
		// Evicted with the picture.
		let Fetch::Download(temp) = cache.fetch(&key, true, waiter(&tx)) else { panic!() };
		std::fs::write(&temp, b"v2").unwrap();
		cache.finish_with(&key, &temp, Ok(Arrived::File(validators.clone())), 0);
		rx.recv().unwrap().unwrap();
		assert!(validators_path(&dir, &key).is_file());
		put(&cache, "icons/1", 10, 10);
		assert!(!path.exists());
		assert!(!validators_path(&dir, &key).exists());
		// Unchanged, but the copy went meanwhile: a failure, retried in full.
		let Fetch::Download(temp) = cache.fetch(&key, true, waiter(&tx)) else { panic!() };
		cache.finish_with(&key, &temp, Ok(Arrived::Unchanged), 0);
		assert!(rx.recv().unwrap().is_err());
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A picture confirmed unchanged counts as used: others go first.
	#[test]
	fn an_unchanged_picture_counts_as_used() {
		let dir = temp_dir("unchanged-used");
		let cache = Cache::new(&dir);
		let key = picture_key("https://example.com/u.png").unwrap();
		let picture = put(&cache, &key, 400, 1000);
		let icon = put(&cache, "icons/1", 400, 1000);
		let Fetch::Download(temp) = cache.fetch(&key, true, Box::new(|_| {})) else { panic!() };
		cache.finish_with(&key, &temp, Ok(Arrived::Unchanged), 1000);
		put(&cache, "icons/2", 400, 1000);
		assert!(picture.exists(), "the picture confirmed unchanged stays");
		assert!(!icon.exists(), "the least recently used file goes");
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Validators whose picture is gone go at the next start.
	#[test]
	fn orphaned_validators_are_removed() {
		let dir = temp_dir("orphans");
		let key = picture_key("https://example.com/o.png").unwrap();
		let orphan = validators_path(&dir, &key);
		std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
		std::fs::write(&orphan, "etag: \"x\"\n").unwrap();
		let cache = Cache::new(&dir);
		assert_eq!(cache.size(), 0);
		assert!(!orphan.exists());
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn fetch_errors_carry_their_retry_hint() {
		let error = FetchError::from("HTTP 503 Service Unavailable");
		assert_eq!(error.retry, RetryHint::Default);
		assert_eq!(error.to_string(), "HTTP 503 Service Unavailable");
		let gone = FetchError::new("HTTP 404 Not Found", RetryHint::Permanent);
		assert_ne!(gone, FetchError::from("HTTP 404 Not Found"));
		let text = Validators { etag: Some("\"a: b\"".into()), last_modified: None }.to_text();
		assert_eq!(Validators::from_text(&text).etag.as_deref(), Some("\"a: b\""));
	}

	/// Pictures are cached files like the others (counted after a restart).
	#[test]
	fn pictures_survive_restart() {
		let dir = temp_dir("pictures");
		let key = picture_key("https://example.com/b.png").unwrap();
		let path = put(&Cache::new(&dir), &key, 10, 0);
		let again = Cache::new(&dir);
		assert_eq!(again.size(), 10);
		assert_eq!(again.get(&key), Some(path));
		std::fs::remove_dir_all(dir).unwrap();
	}
}
