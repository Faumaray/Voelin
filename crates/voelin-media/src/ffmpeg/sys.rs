//! The FFmpeg libraries, found and opened at runtime (`libloading`), and the
//! few functions and struct fields this crate uses.
//!
//! Nothing is linked: every function is looked up by name once, and a
//! library that lacks one is not used. Struct fields are read or written only
//! where FFmpeg offers no function or AVOption for them; see [`super::layout`]
//! for which ones and why that is safe across major versions.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};

use libloading::Library;

/// An FFmpeg object behind an opaque pointer (`AVCodecContext *`, ...).
pub type Ptr = *mut c_void;

/// `va_list` as FFmpeg's log callback receives it and `av_log_format_line2`
/// takes it. On every ABI this crate builds for it is passed as one pointer:
/// a `char *` (Windows, Apple, 32-bit x86), an array that decays to a pointer
/// (x86-64 System V), or a struct larger than 16 bytes passed by reference
/// (AArch64 AAPCS), or a one-word struct in a register (32-bit ARM). It is
/// only ever handed back to FFmpeg, never read.
pub type VaList = *mut c_void;

/// `AVRational`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rational {
	pub num: c_int,
	pub den: c_int,
}

/// The leading fields of `AVPacket`, unchanged since libavcodec 55 (FFmpeg
/// 2.1): `buf`, `pts`, `dts`, `data`, `size`, `stream_index`, `flags`.
/// Packets are always allocated by `av_packet_alloc`, so the struct is at
/// least this large.
#[repr(C)]
pub struct PacketHead {
	pub buf: Ptr,
	pub pts: i64,
	pub dts: i64,
	pub data: *mut u8,
	pub size: c_int,
	pub stream_index: c_int,
	pub flags: c_int,
}

/// `AV_PKT_FLAG_KEY`.
pub const PKT_FLAG_KEY: c_int = 1;

/// The number of data pointers of an `AVFrame` (`AV_NUM_DATA_POINTERS`,
/// 8 since libavutil 51).
pub const NUM_DATA_POINTERS: usize = 8;

/// The leading fields of `AVFrame`, unchanged since libavutil 51 (FFmpeg
/// 0.9): `data[8]`, `linesize[8]`, `extended_data`, `width`, `height`,
/// `nb_samples`, `format`. Frames are always allocated by `av_frame_alloc`.
#[repr(C)]
pub struct FrameHead {
	pub data: [*mut u8; NUM_DATA_POINTERS],
	pub linesize: [c_int; NUM_DATA_POINTERS],
	pub extended_data: *mut *mut u8,
	pub width: c_int,
	pub height: c_int,
	pub nb_samples: c_int,
	pub format: c_int,
}

/// The leading fields of `AVOption` (`name`, `help`, `offset`, `type`),
/// unchanged since FFmpeg 0.5. Only `offset` is read, to locate fields of
/// `AVCodecContext` that have no option of their own (see
/// [`super::layout`]).
#[repr(C)]
pub struct OptionHead {
	pub name: *const c_char,
	pub help: *const c_char,
	pub offset: c_int,
	pub kind: c_int,
}

/// The leading fields of `AVBufferRef` (`buffer`, `data`); `size` changed
/// from `int` to `size_t` in libavutil 57 and is not read.
#[repr(C)]
pub struct BufferRefHead {
	pub buffer: Ptr,
	pub data: *mut u8,
}

/// The leading fields of `AVIOContext` (`av_class`, `buffer`), unchanged
/// since libavformat 53. Only `buffer` is read, to free the buffer of a
/// context made with `avio_alloc_context`.
#[repr(C)]
pub struct AvioHead {
	pub av_class: Ptr,
	pub buffer: *mut u8,
}

/// `AVDRMObjectDescriptor` (`libavutil/hwcontext_drm.h`, unchanged since it
/// was added in libavutil 55).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmObject {
	pub fd: c_int,
	pub size: usize,
	pub format_modifier: u64,
}

/// `AVDRMPlaneDescriptor`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmPlane {
	pub object_index: c_int,
	pub offset: isize,
	pub pitch: isize,
}

/// `AV_DRM_MAX_PLANES`.
pub const DRM_MAX_PLANES: usize = 4;

/// `AVDRMLayerDescriptor`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmLayer {
	pub format: u32,
	pub nb_planes: c_int,
	pub planes: [DrmPlane; DRM_MAX_PLANES],
}

/// `AVDRMFrameDescriptor`: what `data[0]` of an `AV_PIX_FMT_DRM_PRIME` frame
/// points to.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmFrameDescriptor {
	pub nb_objects: c_int,
	pub objects: [DrmObject; DRM_MAX_PLANES],
	pub nb_layers: c_int,
	pub layers: [DrmLayer; DRM_MAX_PLANES],
}

/// `AV_PICTURE_TYPE_NONE` and `AV_PICTURE_TYPE_I`.
pub const PICTURE_TYPE_NONE: c_int = 0;
pub const PICTURE_TYPE_I: c_int = 1;

/// `AV_NOPTS_VALUE`.
pub const NOPTS_VALUE: i64 = i64::MIN;

/// `AV_LOG_*` levels.
pub const LOG_QUIET: c_int = -8;
pub const LOG_ERROR: c_int = 16;
pub const LOG_WARNING: c_int = 24;

/// `AV_HWFRAME_MAP_READ` and `AV_HWFRAME_MAP_DIRECT`.
pub const HWFRAME_MAP_READ: c_int = 1;
pub const HWFRAME_MAP_DIRECT: c_int = 8;

/// `AVERROR(EAGAIN)`: FFmpeg errors are negated `errno` values of the C
/// library it was built against.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd"))]
pub const EAGAIN: c_int = -35;
#[cfg(not(any(
	target_os = "macos",
	target_os = "ios",
	target_os = "freebsd",
	target_os = "openbsd"
)))]
pub const EAGAIN: c_int = -11;

/// `FFERRTAG(a, b, c, d)`.
const fn err_tag(a: u8, b: u8, c: u8, d: u8) -> c_int {
	-((a as c_int) | (b as c_int) << 8 | (c as c_int) << 16 | (d as c_int) << 24)
}

/// `AVERROR_EOF`.
pub const EOF: c_int = err_tag(b'E', b'O', b'F', b' ');
/// `AVERROR_OPTION_NOT_FOUND`.
pub const OPTION_NOT_FOUND: c_int = err_tag(0xF8, b'O', b'P', b'T');

/// Declares a function table: every function, its library, its C
/// signature. `resolve` looks all of them up; a missing one fails the load.
macro_rules! api {
	($(#[$doc:meta])* $api:ident { $($lib:ident { $(fn $name:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)* })* }) => {
		$(#[$doc])*
		#[allow(non_snake_case, dead_code)]
		pub struct $api {
			$($(pub $name: unsafe extern "C" fn($($ty),*) $(-> $ret)?,)*)*
		}

		impl $api {
			/// # Safety
			/// The libraries must be FFmpeg's, so the symbols have the
			/// declared signatures.
			unsafe fn resolve(libs: &Libs) -> Result<Self, String> {
				Ok(Self {
					$($($name: unsafe { libs.symbol(Lib::$lib, stringify!($name))? },)*)*
				})
			}
		}
	};
}

api! {
	/// FFmpeg functions used by this crate (libavutil and libavcodec).
	Api {
	Util {
	fn avutil_version() -> c_uint;
	fn av_version_info() -> *const c_char;
	fn av_frame_alloc() -> Ptr;
	fn av_frame_free(frame: *mut Ptr);
	fn av_frame_get_buffer(frame: Ptr, align: c_int) -> c_int;
	fn av_frame_make_writable(frame: Ptr) -> c_int;
	fn av_frame_unref(frame: Ptr);
	fn av_frame_ref(dst: Ptr, src: Ptr) -> c_int;
	fn av_opt_set(obj: Ptr, name: *const c_char, val: *const c_char, flags: c_int) -> c_int;
	fn av_opt_set_int(obj: Ptr, name: *const c_char, val: i64, flags: c_int) -> c_int;
	fn av_opt_set_q(obj: Ptr, name: *const c_char, val: Rational, flags: c_int) -> c_int;
	fn av_opt_set_image_size(obj: Ptr, name: *const c_char, w: c_int, h: c_int, flags: c_int) -> c_int;
	fn av_opt_set_pixel_fmt(obj: Ptr, name: *const c_char, fmt: c_int, flags: c_int) -> c_int;
	fn av_opt_find(
		obj: Ptr,
		name: *const c_char,
		unit: *const c_char,
		opt_flags: c_int,
		search_flags: c_int
	) -> *const OptionHead;
	fn av_opt_child_next(obj: Ptr, prev: Ptr) -> Ptr;
	fn av_strerror(errnum: c_int, buf: *mut c_char, size: usize) -> c_int;
	fn av_get_pix_fmt(name: *const c_char) -> c_int;
	fn av_get_pix_fmt_name(fmt: c_int) -> *const c_char;
	fn av_log_set_level(level: c_int);
	fn av_log_set_callback(callback: Option<LogCallback>);
	fn av_log_format_line2(
		avcl: Ptr,
		level: c_int,
		fmt: *const c_char,
		vl: VaList,
		line: *mut c_char,
		size: c_int,
		print_prefix: *mut c_int
	) -> c_int;
	fn av_buffer_ref(buf: Ptr) -> Ptr;
	fn av_buffer_unref(buf: *mut Ptr);
	// `size` is an `int` up to libavutil 56: passed in the same register.
	fn av_buffer_allocz(size: usize) -> Ptr;
	fn av_hwdevice_find_type_by_name(name: *const c_char) -> c_int;
	fn av_hwdevice_ctx_create(
		device: *mut Ptr,
		kind: c_int,
		name: *const c_char,
		opts: Ptr,
		flags: c_int
	) -> c_int;
	fn av_hwframe_ctx_alloc(device: Ptr) -> Ptr;
	fn av_hwframe_ctx_init(frames: Ptr) -> c_int;
	fn av_hwframe_get_buffer(frames: Ptr, frame: Ptr, flags: c_int) -> c_int;
	fn av_hwframe_transfer_data(dst: Ptr, src: Ptr, flags: c_int) -> c_int;
	fn av_hwframe_map(dst: Ptr, src: Ptr, flags: c_int) -> c_int;
	fn av_get_sample_fmt(name: *const c_char) -> c_int;
	fn av_malloc(size: usize) -> *mut c_void;
	fn av_freep(ptr: *mut c_void);
	fn av_dict_set(dict: *mut Ptr, key: *const c_char, value: *const c_char, flags: c_int) -> c_int;
	fn av_dict_free(dict: *mut Ptr);
	}

	Codec {
	fn avcodec_version() -> c_uint;
	fn avcodec_find_encoder_by_name(name: *const c_char) -> Ptr;
	fn avcodec_alloc_context3(codec: Ptr) -> Ptr;
	fn avcodec_free_context(ctx: *mut Ptr);
	fn avcodec_open2(ctx: Ptr, codec: Ptr, options: *mut Ptr) -> c_int;
	fn avcodec_send_frame(ctx: Ptr, frame: Ptr) -> c_int;
	fn avcodec_receive_packet(ctx: Ptr, packet: Ptr) -> c_int;
	fn av_packet_alloc() -> Ptr;
	fn av_packet_free(packet: *mut Ptr);
	fn av_packet_unref(packet: Ptr);
	fn avcodec_find_decoder_by_name(name: *const c_char) -> Ptr;
	fn avcodec_send_packet(ctx: Ptr, packet: Ptr) -> c_int;
	fn avcodec_receive_frame(ctx: Ptr, frame: Ptr) -> c_int;
	}
	}
}

api! {
	/// libavformat's I/O functions, for RTMP output: the connection is
	/// libavformat's (`rtmp://`, `rtmps://`); the FLV inside it is ours
	/// (`studio::output::flv`), so no muxer struct is ever touched.
	FormatApi {
	Format {
	fn avformat_version() -> c_uint;
	fn avformat_network_init() -> c_int;
	fn avio_open2(
		ctx: *mut Ptr,
		url: *const c_char,
		flags: c_int,
		interrupt: *const InterruptCallback,
		options: *mut Ptr
	) -> c_int;
	fn avio_write(ctx: Ptr, data: *const u8, size: c_int);
	fn avio_flush(ctx: Ptr);
	fn avio_closep(ctx: *mut Ptr) -> c_int;
	fn avio_alloc_context(
		buffer: *mut u8,
		size: c_int,
		write_flag: c_int,
		opaque: Ptr,
		read: Option<IoCallback>,
		write: Option<IoCallback>,
		seek: Option<SeekCallback>
	) -> Ptr;
	fn avio_context_free(ctx: *mut Ptr);
	}
	}
}

/// `int (*)(void *opaque, uint8_t *buf, int size)`: `read_packet` and
/// `write_packet` of `avio_alloc_context` (`const uint8_t *` for writing
/// since libavformat 61, the same in the ABI).
pub type IoCallback = unsafe extern "C" fn(Ptr, *mut u8, c_int) -> c_int;
/// `int64_t (*)(void *opaque, int64_t offset, int whence)`.
pub type SeekCallback = unsafe extern "C" fn(Ptr, i64, c_int) -> i64;

/// `AVIOInterruptCB` (`int (*callback)(void *); void *opaque;`), unchanged
/// since it was added (libavformat 53). FFmpeg calls it while it waits for
/// the network; a non-zero return aborts the operation.
#[repr(C)]
pub struct InterruptCallback {
	pub callback: Option<unsafe extern "C" fn(Ptr) -> c_int>,
	pub opaque: Ptr,
}

/// `AVIO_FLAG_WRITE`.
pub const AVIO_FLAG_WRITE: c_int = 2;

/// `void (*)(void *avcl, int level, const char *fmt, va_list vl)`.
pub type LogCallback = unsafe extern "C" fn(Ptr, c_int, *const c_char, VaList);

#[derive(Clone, Copy)]
enum Lib {
	Util,
	Codec,
	Format,
}

/// The opened libraries. Never closed: FFmpeg keeps global state (codec
/// registrations, the log callback) that must outlive every object.
pub struct Libs {
	avcodec: Library,
	/// libavutil where symbols are not found through libavcodec's handle
	/// (Windows: `GetProcAddress` does not search dependencies).
	avutil: Option<Library>,
	/// libavformat, if present (RTMP output).
	pub avformat: Option<Library>,
	/// Where libavcodec was found.
	pub path: PathBuf,
}

impl Libs {
	/// libavformat's functions, if libavformat was found and has them all.
	pub fn format_api(&self) -> Result<FormatApi, String> {
		if self.avformat.is_none() {
			return Err(format!("no libavformat of the release of {}", self.path.display()));
		}
		// SAFETY: the library is libavformat (found by its file name, of
		// libavcodec's major), whose functions have the declared signatures.
		unsafe { FormatApi::resolve(self) }
	}

	/// # Safety
	/// `T` must be the symbol's actual type.
	unsafe fn symbol<T: Copy>(&self, lib: Lib, name: &str) -> Result<T, String> {
		let library = match lib {
			Lib::Util => self.avutil.as_ref().unwrap_or(&self.avcodec),
			Lib::Codec => &self.avcodec,
			Lib::Format => self.avformat.as_ref().ok_or("no libavformat")?,
		};
		// SAFETY: the caller guarantees the type; the library stays loaded
		// for the life of the process (it is never dropped, see `Libs`).
		unsafe { library.get::<T>(name.as_bytes()) }
			.map(|s| *s)
			.map_err(|e| format!("{} lacks {name}: {e}", self.path.display()))
	}
}

/// Where to look for FFmpeg.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
	/// Directories searched first (full paths, newest major first).
	pub dirs: Vec<PathBuf>,
	/// Also the system's library search path (and `ldconfig -p` on Linux,
	/// Homebrew and MacPorts on macOS, the executable's directory and `PATH`
	/// on Windows).
	pub system: bool,
}

impl Search {
	/// `VOELIN_FFMPEG_DIR` (only that directory) if set, else the system.
	pub fn from_env() -> Self {
		match std::env::var_os("VOELIN_FFMPEG_DIR") {
			Some(dir) if !dir.is_empty() => Self { dirs: vec![dir.into()], system: false },
			_ => Self { dirs: Vec::new(), system: true },
		}
	}
}

/// The major version in a library file name: `libavcodec.so.61` → 61,
/// `avcodec-61.dll` → 61, `libavcodec.61.dylib` → 61.
pub fn major_of(name: &str) -> Option<u32> {
	let name = name.rsplit(['/', '\\']).next()?;
	let rest = name.strip_prefix("lib").unwrap_or(name);
	let rest = rest.split_once(['.', '-'])?.1;
	rest.split(['.', '-']).find_map(|part| part.parse().ok())
}

/// Library names (not paths) of `base` to try with the system search, newest
/// first. The range of majors reaches well past today's (62) so newer
/// releases are found without a change here; `ldconfig -p` covers the rest.
fn system_names(base: &str) -> Vec<String> {
	const MAJORS: std::ops::RangeInclusive<u32> = 54..=80;
	let mut names = Vec::new();
	if cfg!(windows) {
		names.extend(MAJORS.rev().map(|n| format!("{base}-{n}.dll")));
		names.push(format!("{base}.dll"));
	} else if cfg!(target_vendor = "apple") {
		names.extend(MAJORS.rev().map(|n| format!("lib{base}.{n}.dylib")));
		names.push(format!("lib{base}.dylib"));
	} else {
		names.extend(MAJORS.rev().map(|n| format!("lib{base}.so.{n}")));
		names.push(format!("lib{base}.so"));
	}
	names
}

/// Full paths of `base` in `dir`, newest major first.
fn files_in(dir: &Path, base: &str) -> Vec<PathBuf> {
	let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
	let mut found: Vec<(u32, PathBuf)> = entries
		.flatten()
		.filter_map(|e| {
			let name = e.file_name().to_string_lossy().into_owned();
			let matches = (name.starts_with(&format!("lib{base}.")) && name.contains(".so"))
				|| (name.starts_with(&format!("lib{base}.")) && name.ends_with(".dylib"))
				|| (name.starts_with(&format!("{base}-")) && name.ends_with(".dll"))
				|| name == format!("{base}.dll");
			matches.then(|| (major_of(&name).unwrap_or(0), e.path()))
		})
		.collect();
	found.sort_by(|a, b| b.0.cmp(&a.0));
	found.into_iter().map(|(_, p)| p).collect()
}

/// `ldconfig -p` entries of `base` (Linux), newest major first.
#[cfg(target_os = "linux")]
fn ldconfig(base: &str) -> Vec<PathBuf> {
	let Ok(output) = std::process::Command::new("ldconfig").arg("-p").output() else {
		return Vec::new();
	};
	let text = String::from_utf8_lossy(&output.stdout);
	let prefix = format!("lib{base}.so");
	let mut found: Vec<(u32, PathBuf)> = text
		.lines()
		.filter_map(|line| {
			let (name, path) = line.trim().split_once(" => ")?;
			let name = name.split_whitespace().next()?;
			name.starts_with(&prefix).then(|| (major_of(name).unwrap_or(0), PathBuf::from(path)))
		})
		.collect();
	found.sort_by(|a, b| b.0.cmp(&a.0));
	found.into_iter().map(|(_, p)| p).collect()
}

/// Candidates for `base` (`avcodec`, `avformat`) in search order.
fn candidates(search: &Search, base: &str) -> Vec<PathBuf> {
	let mut list: Vec<PathBuf> = Vec::new();
	for dir in &search.dirs {
		list.extend(files_in(dir, base));
	}
	if search.system {
		if cfg!(target_vendor = "apple") {
			for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/opt/local/lib"] {
				list.extend(files_in(Path::new(dir), base));
			}
		}
		if cfg!(windows)
			&& let Some(dir) =
				std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_owned))
		{
			list.extend(files_in(&dir, base));
		}
		list.extend(system_names(base).into_iter().map(PathBuf::from));
		#[cfg(target_os = "linux")]
		list.extend(ldconfig(base));
	}
	let mut seen = std::collections::HashSet::new();
	list.retain(|p| seen.insert(p.clone()));
	list
}

/// Open one library file or name.
fn open(path: &Path) -> Result<Library, libloading::Error> {
	#[cfg(windows)]
	if path.is_absolute() {
		use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library as WinLibrary};
		// SAFETY: loading runs the library's initialisers; FFmpeg's have no
		// preconditions.
		return unsafe { WinLibrary::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }
			.map(Library::from);
	}
	// SAFETY: as above.
	unsafe { Library::new(path.as_os_str()) }
}

/// Load libavutil files of `dir` first, so libavcodec's dependencies resolve
/// to them when `dir` is not on the system search path (each file only
/// satisfies its own soname).
fn preload_dependencies(dir: &Path, keep: &mut Vec<Library>) {
	for base in ["avutil", "swresample"] {
		for path in files_in(dir, base) {
			if let Ok(lib) = open(&path) {
				keep.push(lib);
			}
		}
	}
}

/// On Windows, the libavutil DLL libavcodec loaded (already in the process).
#[cfg(windows)]
fn loaded_avutil(avcodec: &Path) -> Option<Library> {
	use libloading::os::windows::Library as WinLibrary;
	let mut names: Vec<PathBuf> = system_names("avutil").into_iter().map(PathBuf::from).collect();
	if let Some(dir) = avcodec.parent() {
		names.splice(0..0, files_in(dir, "avutil"));
	}
	names.into_iter().find_map(|name| {
		let file = name.file_name()?.to_owned();
		WinLibrary::open_already_loaded(file).ok().map(Library::from)
	})
}

/// Find and open libavcodec (with the libavutil it depends on) and, if
/// present, libavformat, then look up every function.
pub fn load(search: &Search) -> Result<(Libs, Api), String> {
	let mut errors = Vec::new();
	let mut preloaded = Vec::new();
	for dir in &search.dirs {
		preload_dependencies(dir, &mut preloaded);
	}
	for path in candidates(search, "avcodec") {
		let avcodec = match open(&path) {
			Ok(lib) => lib,
			Err(e) => {
				if path.is_absolute() {
					errors.push(format!("{}: {e}", path.display()));
				}
				continue;
			}
		};
		#[cfg(windows)]
		let avutil = loaded_avutil(&path);
		#[cfg(not(windows))]
		// dlsym on libavcodec's handle also searches its dependencies, so
		// libavutil's functions come from the very library it was built with.
		let avutil = None;
		let mut libs = Libs { avcodec, avutil, avformat: None, path: path.clone() };
		// SAFETY: the library is libavcodec (found by its file name), whose
		// exported functions have the signatures declared in `api!`.
		match unsafe { Api::resolve(&libs) } {
			Ok(api) => {
				// libavformat of the same release: libavformat and libavcodec
				// share their major version.
				// SAFETY: no arguments; returns a version number.
				let major = unsafe { (api.avcodec_version)() } >> 16;
				libs.avformat = candidates(search, "avformat")
					.into_iter()
					.filter(|p| major_of(&p.to_string_lossy()) == Some(major))
					.find_map(|p| open(&p).ok());
				// Keep the preloaded dependencies loaded for good.
				std::mem::forget(preloaded);
				return Ok((libs, api));
			}
			Err(e) => errors.push(e),
		}
	}
	if errors.is_empty() {
		let place = if search.system {
			"on the system library path".to_owned()
		} else {
			search.dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
		};
		Err(format!("no FFmpeg libraries (libavcodec) found {place}"))
	} else {
		Err(errors.join("; "))
	}
}

/// A C string for an FFmpeg call (names and option values never contain
/// NUL; one that does is passed as empty).
pub fn cstr(s: &str) -> CString {
	CString::new(s).unwrap_or_default()
}

impl Api {
	/// FFmpeg's text for an error code.
	pub fn error_text(&self, code: c_int) -> String {
		let mut buf = [0 as c_char; 256];
		// SAFETY: the buffer and its size are valid; av_strerror always
		// NUL-terminates within it.
		unsafe { (self.av_strerror)(code, buf.as_mut_ptr(), buf.len()) };
		// SAFETY: NUL-terminated above.
		let text = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
		format!("{text} ({code})")
	}

	/// `av_get_pix_fmt(name)`: pixel format numbers moved between majors, so
	/// they are always looked up by name. `None` if unknown.
	pub fn pix_fmt(&self, name: &str) -> Option<c_int> {
		let name = cstr(name);
		// SAFETY: a valid C string.
		let fmt = unsafe { (self.av_get_pix_fmt)(name.as_ptr()) };
		(fmt >= 0).then_some(fmt)
	}

	/// The name of pixel format `fmt` (for messages).
	pub fn pix_fmt_name(&self, fmt: c_int) -> String {
		// SAFETY: returns a static string, or NULL for an unknown format.
		let name = unsafe { (self.av_get_pix_fmt_name)(fmt) };
		if name.is_null() {
			return format!("pixel format {fmt}");
		}
		// SAFETY: a NUL-terminated static string.
		unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn majors_from_file_names() {
		assert_eq!(major_of("libavcodec.so.61"), Some(61));
		assert_eq!(major_of("/usr/lib/x86_64-linux-gnu/libavcodec.so.60.31.102"), Some(60));
		assert_eq!(major_of("avcodec-62.dll"), Some(62));
		assert_eq!(major_of("C:\\ffmpeg\\bin\\avcodec-61.dll"), Some(61));
		assert_eq!(major_of("libavcodec.59.dylib"), Some(59));
		assert_eq!(major_of("libavcodec.so"), None);
	}

	#[test]
	fn system_names_are_newest_first_and_unbounded_below_today() {
		let names = system_names("avcodec");
		let majors: Vec<u32> = names.iter().filter_map(|n| major_of(n)).collect();
		assert!(majors.windows(2).all(|w| w[0] > w[1]));
		assert!(majors.contains(&58) && majors.contains(&62) && majors[0] > 62);
	}

	#[test]
	fn error_tags() {
		assert_eq!(EOF, -0x2046_4F45);
		assert_eq!(OPTION_NOT_FOUND, -0x5450_4FF8);
	}

	#[test]
	fn missing_directory_fails_cleanly() {
		let search = Search { dirs: vec!["/nonexistent/voelin-ffmpeg".into()], system: false };
		let err = load(&search).err().expect("nothing there");
		assert!(err.contains("no FFmpeg libraries"), "{err}");
	}
}
