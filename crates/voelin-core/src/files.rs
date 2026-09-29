//! Files in channels, avatars and icons (voice connection, TeamSpeak 3 and 6).
//!
//! # Contract for the UI
//!
//! File browser requests carry a [`RequestId`] the caller picks, transfers
//! a [`TransferId`]; answers carry them back:
//!
//! - [`crate::Command::ListFiles`] → [`crate::Event::FileList`] (an empty
//!   directory is an empty list, not an error).
//! - [`crate::Command::DeleteFiles`], [`crate::Command::RenameFile`],
//!   [`crate::Command::CreateDirectory`] → [`crate::Event::RequestDone`].
//! - [`crate::Command::DownloadFile`], [`crate::Command::DownloadChatFile`]
//!   (a file linked in chat, [`voelin_model::FileRef`]),
//!   [`crate::Command::UploadFile`] → [`crate::Event::Transfer`]:
//!   `Requested`, `Started`, `Progress` (every `files.progress_interval_ms`),
//!   then `Done`, `Failed` or `Cancelled` ([`crate::Command::CancelTransfer`]).
//!
//! Downloads stream to disk: into `<path>.part`, renamed to `<path>` when
//! complete (with `resume`, an existing `.part` is continued). Downloads
//! into memory ([`DownloadTo::Memory`]) come back in `Done`. Uploads read
//! the file as they send it. There is no size limit besides the server's
//! quotas.
//!
//! Paths on the server start with `/` (`/docs/report.pdf`); channel 0 holds
//! the server's avatars and icons.
//!
//! Avatars and icons are fetched by themselves (setting
//! `cache.fetch_images`) into the engine's cache ([`crate::cache`]):
//! [`crate::Event::AvatarReady`] when a client's avatar is there (again when
//! its hash changes), [`crate::Event::IconReady`] for every icon a server,
//! group, channel or client uses (ids below 1000 are built into clients and
//! never downloaded). [`crate::Command::SetAvatar`] uploads our own.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Picked by the caller; the answer carries it back.
pub type RequestId = u64;
/// Picked by the caller; the transfer's events carry it.
pub type TransferId = u64;

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileEntry {
	pub name: String,
	/// Bytes (0 for directories).
	pub size: u64,
	/// Unix seconds of the last change.
	pub modified_s: i64,
	pub is_dir: bool,
}

/// Bytes of a download into memory.
#[derive(Clone, PartialEq, Eq)]
pub struct Bytes(pub Arc<[u8]>);

impl fmt::Debug for Bytes {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Bytes({} bytes)", self.0.len())
	}
}

/// Where a download goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadTo {
	/// A file (written as `<path>.part` until complete). `resume`: continue
	/// an existing `.part`.
	Path { path: PathBuf, resume: bool },
	/// Memory: the bytes come in [`TransferState::Done`].
	Memory,
}

/// A transfer's progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
	/// Asked the server.
	Requested,
	/// Data flows; `offset` bytes were there before (resume).
	Started {
		size: u64,
		offset: u64,
	},
	Progress {
		done: u64,
		size: u64,
	},
	Done {
		size: u64,
		path: Option<PathBuf>,
		data: Option<Bytes>,
	},
	Failed(String),
	Cancelled,
}

impl TransferState {
	pub fn is_final(&self) -> bool {
		matches!(self, Self::Done { .. } | Self::Failed(_) | Self::Cancelled)
	}
}

/// Reports a transfer's states.
pub(crate) type Report = Arc<dyn Fn(TransferState) + Send + Sync>;

/// Where a running download writes.
#[derive(Clone, Debug)]
pub(crate) enum Sink {
	/// Write `part` (appending with `append`), then rename it to `dest`.
	File {
		part: PathBuf,
		dest: PathBuf,
		append: bool,
	},
	Memory,
}

impl Sink {
	pub fn from_target(to: &DownloadTo) -> Self {
		match to {
			DownloadTo::Path { path, resume } => {
				let mut part = path.clone().into_os_string();
				part.push(".part");
				Sink::File { part: part.into(), dest: path.clone(), append: *resume }
			}
			DownloadTo::Memory => Sink::Memory,
		}
	}

	/// Bytes already there to resume from.
	pub fn offset(&self) -> u64 {
		match self {
			Sink::File { part, append: true, .. } => std::fs::metadata(part).map_or(0, |m| m.len()),
			_ => 0,
		}
	}

	/// The partial file to remove when a transfer is cancelled.
	pub fn part(&self) -> Option<&Path> {
		match self {
			Sink::File { part, .. } => Some(part),
			Sink::Memory => None,
		}
	}
}

/// Rate limit for progress reports.
struct Progress<'a> {
	report: &'a Report,
	every: Duration,
	last: Instant,
}

impl Progress<'_> {
	fn update(&mut self, done: u64, size: u64) {
		if self.last.elapsed() >= self.every {
			self.last = Instant::now();
			(self.report)(TransferState::Progress { done, size });
		}
	}
}

const CHUNK: usize = 64 * 1024;

/// Receive a file: `size` bytes in total, `offset` of them already in the
/// sink. Reports `Started`, `Progress` and the final state.
pub(crate) async fn download(
	mut stream: TcpStream,
	size: u64,
	offset: u64,
	sink: Sink,
	progress_every: Duration,
	report: Report,
) {
	report(TransferState::Started { size, offset });
	let result = receive(&mut stream, size, offset, &sink, progress_every, &report).await;
	report(match result {
		Ok(done) => done,
		Err(e) => TransferState::Failed(e),
	});
}

async fn receive(
	stream: &mut TcpStream,
	size: u64,
	offset: u64,
	sink: &Sink,
	progress_every: Duration,
	report: &Report,
) -> Result<TransferState, String> {
	let mut progress = Progress { report, every: progress_every, last: Instant::now() };
	let mut buf = vec![0u8; CHUNK];
	let mut done = offset;
	match sink {
		Sink::File { part, dest, append } => {
			if let Some(dir) = part.parent().filter(|d| !d.as_os_str().is_empty()) {
				tokio::fs::create_dir_all(dir)
					.await
					.map_err(|e| format!("{}: {e}", dir.display()))?;
			}
			let mut file = tokio::fs::OpenOptions::new()
				.create(true)
				.write(true)
				.append(*append)
				.truncate(!*append)
				.open(part)
				.await
				.map_err(|e| format!("{}: {e}", part.display()))?;
			let received = async {
				while done < size {
					let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
					if n == 0 {
						return Err(format!("connection closed after {done} of {size} bytes"));
					}
					let data = &buf[..n];
					file.write_all(data).await.map_err(|e| format!("{}: {e}", part.display()))?;
					done += n as u64;
					progress.update(done, size);
				}
				Ok(())
			}
			.await;
			// What arrived stays in the part (to resume), also after an error.
			let flushed = file.flush().await.map_err(|e| e.to_string());
			drop(file);
			received?;
			flushed?;
			if part != dest {
				tokio::fs::rename(part, dest)
					.await
					.map_err(|e| format!("{}: {e}", dest.display()))?;
			}
			Ok(TransferState::Done { size, path: Some(dest.clone()), data: None })
		}
		Sink::Memory => {
			let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(1 << 24));
			while done < size {
				let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
				if n == 0 {
					return Err(format!("connection closed after {done} of {size} bytes"));
				}
				data.extend_from_slice(&buf[..n]);
				done += n as u64;
				progress.update(done, size);
			}
			Ok(TransferState::Done { size, path: None, data: Some(Bytes(data.into())) })
		}
	}
}

/// Send `file` from `offset` (`size` bytes in total). Reports `Started`,
/// `Progress` and the final state.
pub(crate) async fn upload(
	mut stream: TcpStream,
	file: PathBuf,
	size: u64,
	offset: u64,
	progress_every: Duration,
	report: Report,
) {
	report(TransferState::Started { size, offset });
	let result = send(&mut stream, &file, size, offset, progress_every, &report).await;
	report(match result {
		Ok(()) => TransferState::Done { size, path: Some(file), data: None },
		Err(e) => TransferState::Failed(e),
	});
}

async fn send(
	stream: &mut TcpStream,
	path: &Path,
	size: u64,
	offset: u64,
	progress_every: Duration,
	report: &Report,
) -> Result<(), String> {
	let mut progress = Progress { report, every: progress_every, last: Instant::now() };
	let mut file =
		tokio::fs::File::open(path).await.map_err(|e| format!("{}: {e}", path.display()))?;
	if offset > 0 {
		file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| e.to_string())?;
	}
	let mut buf = vec![0u8; CHUNK];
	let mut done = offset;
	while done < size {
		let want = usize::try_from(size - done).unwrap_or(CHUNK).min(CHUNK);
		let n =
			file.read(&mut buf[..want]).await.map_err(|e| format!("{}: {e}", path.display()))?;
		if n == 0 {
			return Err(format!("{} shrank while uploading", path.display()));
		}
		stream.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
		done += n as u64;
		progress.update(done, size);
	}
	stream.flush().await.map_err(|e| e.to_string())?;
	stream.shutdown().await.map_err(|e| e.to_string())?;
	// The server closes the connection once it has everything.
	let _ = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf[..1])).await;
	Ok(())
}

/// A file's MD5 (hex), as `client_flag_avatar` names an avatar.
pub(crate) fn md5_file(path: &Path) -> std::io::Result<String> {
	use std::io::Read;
	let mut file = std::fs::File::open(path)?;
	let mut ctx = md5::Context::new();
	let mut buf = vec![0u8; CHUNK];
	loop {
		let n = file.read(&mut buf)?;
		if n == 0 {
			break;
		}
		ctx.consume(&buf[..n]);
	}
	Ok(format!("{:x}", ctx.finalize()))
}

/// The avatar file of a client on the server: `/avatar_` and the unique
/// id's bytes as letters `a` to `p` (one per nibble), as TeamSpeak 3 and 6
/// servers store them (verified against 3.13.8 and 6.0.0-beta13.1).
pub fn avatar_path(uid: &str) -> Option<String> {
	use base64::Engine;
	let bytes = base64::engine::general_purpose::STANDARD.decode(uid.trim()).ok()?;
	let mut path = String::with_capacity(8 + bytes.len() * 2);
	path.push_str("/avatar_");
	for b in bytes {
		path.push(char::from(b'a' + (b >> 4)));
		path.push(char::from(b'a' + (b & 0x0f)));
	}
	Some(path)
}

/// The file of an icon on the server.
pub fn icon_path(id: u32) -> String {
	format!("/icon_{id}")
}

/// Icons below this id are built into clients (group icons 100 to 600).
pub const FIRST_DOWNLOADABLE_ICON: u32 = 1000;

/// `dir` and `name` joined as a server path (`/dir/name`).
pub fn join_path(dir: &str, name: &str) -> String {
	let dir = dir.trim_end_matches('/');
	let name = name.trim_start_matches('/');
	format!("{dir}/{name}")
}

/// A server path in the form the server echoes (`/` for the root, no
/// trailing slash otherwise).
pub(crate) fn normalize_dir(path: &str) -> String {
	let trimmed = path.trim_end_matches('/');
	if trimmed.is_empty() {
		"/".into()
	} else if trimmed.starts_with('/') {
		trimmed.into()
	} else {
		format!("/{trimmed}")
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use tokio::net::TcpListener;

	use super::*;

	#[test]
	fn avatar_paths() {
		// Two bytes 0x01 0xfe: "ab" "po".
		assert_eq!(avatar_path("Af4=").as_deref(), Some("/avatar_abpo"));
		assert_eq!(avatar_path("not base64!"), None);
		assert_eq!(icon_path(123456), "/icon_123456");
	}

	#[test]
	fn paths() {
		assert_eq!(join_path("/", "a.txt"), "/a.txt");
		assert_eq!(join_path("/docs/", "/a.txt"), "/docs/a.txt");
		assert_eq!(normalize_dir(""), "/");
		assert_eq!(normalize_dir("/docs/"), "/docs");
		assert_eq!(normalize_dir("docs"), "/docs");
	}

	fn temp(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("voelin-files-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn recorder() -> (Report, Arc<Mutex<Vec<TransferState>>>) {
		let states = Arc::new(Mutex::new(Vec::new()));
		let s = states.clone();
		(Arc::new(move |state| s.lock().unwrap().push(state)), states)
	}

	/// A server that sends `data` on the first connection.
	async fn serve(data: Vec<u8>) -> std::net::SocketAddr {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		tokio::spawn(async move {
			let (mut s, _) = listener.accept().await.unwrap();
			s.write_all(&data).await.unwrap();
		});
		addr
	}

	#[tokio::test]
	async fn downloads_to_disk_and_resumes() {
		let dir = temp("download");
		let dest = dir.join("sub/file.bin");
		let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
		// First half: the connection ends early; the part stays.
		let addr = serve(data[..100_000].to_vec()).await;
		let (report, states) = recorder();
		let target = DownloadTo::Path { path: dest.clone(), resume: false };
		let sink = Sink::from_target(&target);
		let stream = TcpStream::connect(addr).await.unwrap();
		download(stream, data.len() as u64, 0, sink, Duration::ZERO, report).await;
		let last = states.lock().unwrap().last().cloned().unwrap();
		assert!(matches!(last, TransferState::Failed(ref e) if e.contains("100000 of 200000")));
		assert!(!dest.exists());
		// Resume: the rest.
		let target = DownloadTo::Path { path: dest.clone(), resume: true };
		let sink = Sink::from_target(&target);
		assert_eq!(sink.offset(), 100_000);
		let addr = serve(data[100_000..].to_vec()).await;
		let (report, states) = recorder();
		let stream = TcpStream::connect(addr).await.unwrap();
		download(stream, data.len() as u64, 100_000, sink, Duration::ZERO, report).await;
		let states = states.lock().unwrap().clone();
		assert_eq!(states[0], TransferState::Started { size: 200_000, offset: 100_000 });
		assert!(states.iter().any(|s| matches!(s, TransferState::Progress { .. })));
		assert_eq!(
			states.last().unwrap(),
			&TransferState::Done { size: 200_000, path: Some(dest.clone()), data: None }
		);
		assert_eq!(std::fs::read(&dest).unwrap(), data);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn downloads_to_memory() {
		let addr = serve(b"hello".to_vec()).await;
		let (report, states) = recorder();
		let stream = TcpStream::connect(addr).await.unwrap();
		download(stream, 5, 0, Sink::Memory, Duration::from_secs(60), report).await;
		let last = states.lock().unwrap().last().cloned().unwrap();
		let TransferState::Done { data: Some(Bytes(data)), .. } = last else { panic!("{last:?}") };
		assert_eq!(&*data, b"hello");
	}

	#[tokio::test]
	async fn uploads_from_offset() {
		let dir = temp("upload");
		let file = dir.join("up.bin");
		std::fs::write(&file, b"0123456789").unwrap();
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move {
			let (mut s, _) = listener.accept().await.unwrap();
			let mut got = Vec::new();
			s.read_to_end(&mut got).await.unwrap();
			got
		});
		let (report, states) = recorder();
		let stream = TcpStream::connect(addr).await.unwrap();
		upload(stream, file.clone(), 10, 4, Duration::ZERO, report).await;
		assert_eq!(server.await.unwrap(), b"456789");
		assert!(matches!(states.lock().unwrap().last(), Some(TransferState::Done { .. })));
		assert_eq!(md5_file(&file).unwrap(), "781e5e245d69b566979b86e28d23f2c7");
		std::fs::remove_dir_all(dir).unwrap();
	}
}
