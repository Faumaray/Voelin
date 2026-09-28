//! Application audio capture against a private PipeWire and WirePlumber:
//! two child processes play tones (1000 Hz as `voelin-tone-a`, 1500 Hz as
//! `voelin-tone-b`) and this process plays 700 Hz, like our voice playback.
//! "Desktop audio without us" must hear both children and not us, a
//! capture of one application only that one, and a child started later
//! is picked up while capturing.
//!
//! Needs `pipewire`, `wireplumber` and `dbus-daemon` (Debian/Ubuntu:
//! `pipewire-bin wireplumber dbus-daemon`); skipped without them. The
//! daemons run in a temporary `XDG_RUNTIME_DIR` and never touch the
//! user's session.
#![cfg(all(target_os = "linux", feature = "pipewire"))]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use voelin_media::capture::pipewire_links::LinkManager;
use voelin_media::capture::playback::{AppMatch, PlaybackFilter, SourceCapture};
use voelin_media::mix::{BlockClock, MixerConfig, StreamMixer};

/// Set in a child: play this frequency until killed.
const TONE_ENV: &str = "VOELIN_TEST_TONE";
const OURS: f64 = 700.0;
const TONE_A: f64 = 1000.0;
const TONE_B: f64 = 1500.0;
const TONE_C: f64 = 2200.0;

fn find(binary: &str) -> Option<PathBuf> {
	std::env::var_os("PATH")?
		.to_str()?
		.split(':')
		.map(|dir| Path::new(dir).join(binary))
		.find(|p| p.is_file())
}

/// Play `freq` Hz (0.3, stereo, 48 kHz) as application `name` until `stop`.
fn play(remote: Option<String>, freq: f64, name: String, stop: Arc<AtomicBool>) {
	pw::init();
	let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
	let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
	let mut props = pw::properties::PropertiesBox::new();
	if let Some(remote) = remote {
		props.insert(*pw::keys::REMOTE_NAME, remote);
	}
	props.insert(*pw::keys::APP_NAME, name.as_str());
	let core = context.connect_rc(Some(props)).unwrap();
	// End with the daemon.
	let _errors = core
		.add_listener_local()
		.error({
			let mainloop = mainloop.downgrade();
			move |id, _, _, _| {
				if id == pw::core::PW_ID_CORE
					&& let Some(mainloop) = mainloop.upgrade()
				{
					mainloop.quit();
				}
			}
		})
		.register();
	let mut props = pw::properties::PropertiesBox::new();
	props.insert(*pw::keys::MEDIA_TYPE, "Audio");
	props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
	props.insert(*pw::keys::MEDIA_ROLE, "Music");
	props.insert(*pw::keys::APP_NAME, name.as_str());
	props.insert(*pw::keys::NODE_NAME, name.as_str());
	let stream = pw::stream::StreamRc::new(core.clone(), &name, props).unwrap();
	let _listener = stream
		.add_local_listener_with_user_data(0.0f64)
		.process(move |stream, phase| {
			let Some(mut buffer) = stream.dequeue_buffer() else { return };
			let data = &mut buffer.datas_mut()[0];
			let frames = match data.data() {
				Some(bytes) => {
					let frames = bytes.len() / 8;
					for frame in bytes.chunks_exact_mut(8) {
						*phase += std::f64::consts::TAU * freq / 48_000.0;
						let s = (0.3 * phase.sin()) as f32;
						frame[..4].copy_from_slice(&s.to_le_bytes());
						frame[4..].copy_from_slice(&s.to_le_bytes());
					}
					frames
				}
				None => 0,
			};
			let chunk = data.chunk_mut();
			*chunk.offset_mut() = 0;
			*chunk.stride_mut() = 8;
			*chunk.size_mut() = (frames * 8) as u32;
		})
		.register()
		.unwrap();
	let mut info = pw::spa::param::audio::AudioInfoRaw::new();
	info.set_format(pw::spa::param::audio::AudioFormat::F32LE);
	info.set_rate(48_000);
	info.set_channels(2);
	let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
	position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
	position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
	info.set_position(position);
	let format: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
		std::io::Cursor::new(Vec::new()),
		&pw::spa::pod::Value::Object(pw::spa::pod::Object {
			type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
			id: pw::spa::sys::SPA_PARAM_EnumFormat,
			properties: info.into(),
		}),
	)
	.unwrap()
	.0
	.into_inner();
	stream
		.connect(
			pw::spa::utils::Direction::Output,
			None,
			pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
			&mut [pw::spa::pod::Pod::from_bytes(&format).unwrap()],
		)
		.unwrap();
	let timer = mainloop.loop_().add_timer({
		let mainloop = mainloop.downgrade();
		move |_| {
			if stop.load(Ordering::Relaxed)
				&& let Some(mainloop) = mainloop.upgrade()
			{
				mainloop.quit();
			}
		}
	});
	timer
		.update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(50)))
		.into_result()
		.unwrap();
	mainloop.run();
}

/// The child side: `cargo test` runs this test in the child process, where
/// `VOELIN_TEST_TONE` says what to play.
#[test]
fn tone_child() {
	let Ok(spec) = std::env::var(TONE_ENV) else { return };
	let (freq, name) = spec.split_once(':').unwrap();
	play(None, freq.parse().unwrap(), name.to_owned(), Arc::new(AtomicBool::new(false)));
}

/// Processes killed on drop.
struct Daemons {
	children: Vec<Child>,
	dir: PathBuf,
}

impl Drop for Daemons {
	fn drop(&mut self) {
		for child in self.children.iter_mut().rev() {
			let _ = child.kill();
			let _ = child.wait();
		}
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

impl Daemons {
	fn start() -> Option<Self> {
		let (pipewire, wireplumber, dbus) =
			(find("pipewire")?, find("wireplumber")?, find("dbus-daemon")?);
		// Short: the socket path must fit in 108 bytes.
		let dir = PathBuf::from(format!("/tmp/voelin-pw-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let mut daemons = Daemons { children: Vec::new(), dir: dir.clone() };
		let mut bus = Command::new(dbus)
			.args(["--session", "--nofork", "--print-address=1"])
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.spawn()
			.ok()?;
		let mut address = String::new();
		BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).ok()?;
		daemons.children.push(bus);
		let env = |command: &mut Command| {
			command
				.env("XDG_RUNTIME_DIR", &dir)
				.env("XDG_CONFIG_HOME", dir.join("config"))
				.env("XDG_STATE_HOME", dir.join("state"))
				.env("DBUS_SESSION_BUS_ADDRESS", address.trim())
				.env_remove("PIPEWIRE_REMOTE")
				.stdout(Stdio::null())
				.stderr(Stdio::null());
		};
		// A sink, as every desktop has one (WirePlumber sets up the ports of
		// playback streams once they have somewhere to go).
		let conf = dir.join("config/pipewire/pipewire.conf.d");
		std::fs::create_dir_all(&conf).unwrap();
		std::fs::write(
			conf.join("voelin-test-sink.conf"),
			"context.objects = [ { factory = adapter args = { factory.name = support.null-audio-sink \
			 node.name = voelin-test-sink media.class = Audio/Sink audio.position = [ FL FR ] } } ]\n",
		)
		.unwrap();
		let mut command = Command::new(pipewire);
		env(&mut command);
		daemons.children.push(command.spawn().ok()?);
		let socket = dir.join("pipewire-0");
		let deadline = Instant::now() + Duration::from_secs(5);
		while !socket.exists() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(20));
		}
		let mut command = Command::new(wireplumber);
		env(&mut command);
		daemons.children.push(command.spawn().ok()?);
		socket.exists().then_some(daemons)
	}

	fn socket(&self) -> PathBuf {
		self.dir.join("pipewire-0")
	}

	/// Another process playing `freq` as `name`: this test binary, started
	/// through a shell that exits right away, so the player is not our
	/// descendant (our own process tree is excluded from desktop audio).
	fn tone_player(&self, freq: f64, name: &str) -> Player {
		let out = Command::new("sh")
			.args(["-c", "\"$0\" --exact tone_child --test-threads=1 >/dev/null 2>&1 & echo $!"])
			.arg(std::env::current_exe().unwrap())
			.env(TONE_ENV, format!("{freq}:{name}"))
			.env("XDG_RUNTIME_DIR", &self.dir)
			.env_remove("PIPEWIRE_REMOTE")
			.stderr(Stdio::null())
			.output()
			.unwrap();
		let pid = String::from_utf8_lossy(&out.stdout).trim().parse().expect("player pid");
		Player { pid }
	}
}

/// A tone player process; killed on drop.
struct Player {
	pid: u32,
}

impl Player {
	fn kill(&mut self) {
		if self.pid != 0 {
			let _ = Command::new("kill").arg(self.pid.to_string()).status();
			self.pid = 0;
		}
	}
}

impl Drop for Player {
	fn drop(&mut self) {
		self.kill();
	}
}

/// Print the graph (for a failing test), if the PipeWire tools are there.
fn dump(dir: &Path) {
	for (tool, args) in [("pw-link", &["-l"][..]), ("pw-cli", &["ls"][..])] {
		if let Some(tool) = find(tool)
			&& let Ok(out) = Command::new(tool).args(args).env("XDG_RUNTIME_DIR", dir).output()
		{
			eprintln!("{}", String::from_utf8_lossy(&out.stdout));
		}
	}
}

/// Goertzel magnitude of `freq` in the left channel (about amplitude / 2).
fn tone(samples: &[f32], freq: f64) -> f64 {
	let k = 2.0 * (std::f64::consts::TAU * freq / 48_000.0).cos();
	let (mut s1, mut s2) = (0.0, 0.0);
	let mut n = 0;
	for frame in samples.chunks_exact(2) {
		let s0 = f64::from(frame[0]) + k * s1 - s2;
		s2 = s1;
		s1 = s0;
		n += 1;
	}
	(s1 * s1 + s2 * s2 - k * s1 * s2).max(0.0).sqrt() / f64::from(n.max(1))
}

/// A mixer with one capture, mixed in real time on a thread; keeps the last
/// second.
struct Listener {
	stop: Arc<AtomicBool>,
	last: Arc<std::sync::Mutex<Vec<f32>>>,
	thread: Option<JoinHandle<()>>,
	_capture: Box<dyn SourceCapture>,
}

impl Listener {
	fn new(manager: &Arc<LinkManager>, filter: PlaybackFilter) -> Self {
		let mut mixer = StreamMixer::new(MixerConfig::default());
		let source = mixer.handle().add_source(format!("{filter:?}"));
		let capture = manager.capture(&filter, &source).expect("capture");
		let stop = Arc::new(AtomicBool::new(false));
		let last = Arc::new(std::sync::Mutex::new(Vec::new()));
		let thread = std::thread::spawn({
			let (stop, last) = (stop.clone(), last.clone());
			move || {
				let mut clock = BlockClock::new(960);
				let mut block = vec![0.0f32; 1920];
				while !stop.load(Ordering::Relaxed) {
					for _ in clock.due(Instant::now()) {
						mixer.mix(&mut block);
						let mut last = last.lock().unwrap();
						last.extend_from_slice(&block);
						let excess = last.len().saturating_sub(96_000);
						last.drain(..excess);
					}
					std::thread::sleep(Duration::from_millis(5));
				}
			}
		});
		Self { stop, last, thread: Some(thread), _capture: capture }
	}

	fn levels(&self, freqs: &[f64]) -> Vec<f64> {
		let last = self.last.lock().unwrap();
		freqs.iter().map(|&f| tone(&last, f)).collect()
	}

	/// Wait until `heard` are all above 0.05 and `silent` below 0.005.
	fn expect(&self, heard: &[f64], silent: &[f64], what: &str) {
		self.expect_in(heard, silent, what, None);
	}

	fn expect_in(&self, heard: &[f64], silent: &[f64], what: &str, dir: Option<&Path>) {
		let deadline = Instant::now() + Duration::from_secs(15);
		loop {
			let loud = self.levels(heard);
			let quiet = self.levels(silent);
			if loud.iter().all(|&l| l > 0.05) && quiet.iter().all(|&l| l < 0.005) {
				return;
			}
			if Instant::now() >= deadline
				&& let Some(dir) = dir
			{
				dump(dir);
			}
			assert!(
				Instant::now() < deadline,
				"{what}: {heard:?} at {loud:?} (want > 0.05), {silent:?} at {quiet:?} (want < 0.005)"
			);
			std::thread::sleep(Duration::from_millis(200));
		}
	}
}

impl Drop for Listener {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

#[test]
fn desktop_without_self_and_single_apps() {
	if std::env::var_os(TONE_ENV).is_some() {
		return;
	}
	let _ = tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.with_test_writer()
		.try_init();
	let Some(daemons) = Daemons::start() else {
		eprintln!("skipped: needs pipewire, wireplumber and dbus-daemon");
		return;
	};
	let remote = daemons.socket().to_string_lossy().into_owned();
	// Our own playback, as the voice audio would be.
	let stop_ours = Arc::new(AtomicBool::new(false));
	let ours = std::thread::spawn({
		let (remote, stop) = (remote.clone(), stop_ours.clone());
		move || play(Some(remote), OURS, "voelin-own-voices".into(), stop)
	});
	let mut a = daemons.tone_player(TONE_A, "voelin-tone-a");
	let b = daemons.tone_player(TONE_B, "voelin-tone-b");

	let manager = Arc::new(LinkManager::connect(Some(&daemons.socket())).expect("connect"));
	let desktop = Listener::new(&manager, PlaybackFilter::AllButSelf);
	let by_name =
		Listener::new(&manager, PlaybackFilter::App(AppMatch::Name("VOELIN-TONE-A".into())));
	let by_pid = Listener::new(&manager, PlaybackFilter::App(AppMatch::Pid(b.pid)));
	desktop.expect_in(&[TONE_A, TONE_B], &[OURS], "desktop without us", Some(&daemons.dir));
	by_name.expect(&[TONE_A], &[TONE_B, OURS], "app by name");
	by_pid.expect(&[TONE_B], &[TONE_A, OURS], "app by pid");

	// The picker's list: both children with their pids, not us.
	let mut apps = manager.apps();
	let list = apps.borrow_and_update().clone();
	let names: Vec<&str> = list.iter().map(|a| a.name.as_str()).collect();
	assert!(names.contains(&"voelin-tone-a") && names.contains(&"voelin-tone-b"), "{list:?}");
	assert!(!names.contains(&"voelin-own-voices"), "{list:?}");
	let entry = list.iter().find(|a| a.name == "voelin-tone-b").unwrap();
	assert_eq!(entry.pid, Some(b.pid));
	assert!(entry.streams >= 1);

	// A new application is picked up live; one that quits is dropped.
	let c = daemons.tone_player(TONE_C, "voelin-tone-c");
	desktop.expect(&[TONE_A, TONE_B, TONE_C], &[OURS], "desktop after a new app");
	by_name.expect(&[TONE_A], &[TONE_C], "app by name after a new app");
	a.kill();
	desktop.expect(&[TONE_B, TONE_C], &[TONE_A, OURS], "desktop after an app quit");
	let deadline = Instant::now() + Duration::from_secs(10);
	while apps.borrow_and_update().iter().any(|a| a.name == "voelin-tone-a") {
		assert!(Instant::now() < deadline, "voelin-tone-a still listed");
		std::thread::sleep(Duration::from_millis(100));
	}
	// Restarted, the application is matched by name again.
	let a = daemons.tone_player(TONE_A, "voelin-tone-a");
	by_name.expect(&[TONE_A], &[TONE_B, TONE_C, OURS], "app by name after a restart");

	drop((desktop, by_name, by_pid));
	drop((a, b, c));
	stop_ours.store(true, Ordering::Relaxed);
	let _ = ours.join();
	drop(manager);
	drop(daemons);
}
