//! The avatar and icon cache: files on disk named by their content.
//!
//! An avatar's name is its MD5 hash (`client_flag_avatar`), an icon's its
//! id (the CRC32 of the image, as TeamSpeak assigns them), so a lookup is a
//! map access, and the same image seen on several servers or under
//! several clients is one file. Keys are relative paths (`avatars/<md5>`,
//! `icons/<id>`).
//!
//! The cache is bounded by `cache.max_mb` (read on every insert; 0: no
//! limit): the least recently used files go first. Use times survive
//! restarts as the files' modification times.
//!
//! Downloads are deduplicated: [`Cache::fetch`] tells the first asker to
//! download (into a temporary path), later askers wait for the same
//! download ([`Cache::finish`] answers them all).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use tracing::debug;

/// Called with the cached file's path, or why it could not be fetched.
pub(crate) type Waiter = Box<dyn FnOnce(Result<PathBuf, String>) + Send>;

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
	pub(crate) fn fetch(&self, key: &str, waiter: Waiter) -> Fetch {
		let mut index = self.lock();
		if let Some(path) = self.touch(&mut index, key) {
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
		result: Result<(), String>,
		max_bytes: u64,
	) {
		let result = result.and_then(|()| self.insert_file(key, temp, max_bytes));
		let _ = std::fs::remove_file(temp);
		let waiters = self.lock().in_flight.remove(key).unwrap_or_default();
		for waiter in waiters {
			waiter(result.clone());
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
	for sub in ["avatars", "icons"] {
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
		let Fetch::Download(temp) = cache.fetch(key, Box::new(move |r| tx.send(r).unwrap())) else {
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
		let Fetch::Download(temp) = cache.fetch(&key, waiter(tx.clone())) else { panic!() };
		// A second asker waits for the same download.
		assert!(matches!(cache.fetch(&key, waiter(tx.clone())), Fetch::Waiting));
		std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
		std::fs::write(&temp, b"png").unwrap();
		cache.finish(&key, &temp, Ok(()), 0);
		let a = rx.recv().unwrap().unwrap();
		let b = rx.recv().unwrap().unwrap();
		assert_eq!(a, b);
		assert_eq!(std::fs::read(&a).unwrap(), b"png");
		assert!(!temp.exists());
		assert!(matches!(cache.fetch(&key, waiter(tx)), Fetch::Cached));
		assert_eq!(rx.recv().unwrap().unwrap(), a);
		assert_eq!(cache.get(&key), Some(a));
		assert_eq!(cache.size(), 3);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn failed_download_answers_everyone_and_retries() {
		let dir = temp_dir("fail");
		let cache = Cache::new(&dir);
		let (tx, rx) = mpsc::channel();
		let tx2 = tx.clone();
		let Fetch::Download(temp) = cache.fetch("icons/7", Box::new(move |r| tx.send(r).unwrap()))
		else {
			panic!()
		};
		assert!(matches!(
			cache.fetch("icons/7", Box::new(move |r| tx2.send(r).unwrap())),
			Fetch::Waiting
		));
		cache.finish("icons/7", &temp, Err("file not found".into()), 0);
		assert_eq!(rx.recv().unwrap(), Err("file not found".into()));
		assert_eq!(rx.recv().unwrap(), Err("file not found".into()));
		assert!(cache.get("icons/7").is_none());
		assert!(matches!(cache.fetch("icons/7", Box::new(|_| {})), Fetch::Download(_)));
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
	}
}
