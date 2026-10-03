//! `android_main`: the activity's native thread.
//!
//! android-activity calls it on a new thread for every activity instance
//! (the process can outlive activities while voice runs in the service).
//! Process-wide setup happens once; the window is built per activity.

use std::path::Path;
use std::sync::{Mutex, Once, OnceLock, PoisonError};

use anyhow::Context as _;
use slint::android::AndroidApp;
use tracing::{error, info, warn};
use voelin_core::Command;

use crate::foreground::Foreground;
use crate::host::EngineHost;
use crate::{bridge, camera, capture, egl, secrets};

/// Store setting (`bool`): the user's opt-in to local crash reports (read
/// before the settings service starts, straight from the database).
pub const CRASH_REPORTS_SETTING: &str = voelin_core::settings::CRASH_REPORTS_KEY;

static HOST: OnceLock<EngineHost> = OnceLock::new();
static FOREGROUND: Mutex<Option<Foreground>> = Mutex::new(None);

fn host() -> anyhow::Result<&'static EngineHost> {
	if let Some(host) = HOST.get() {
		return Ok(host);
	}
	let host = EngineHost::start().context("starting the engine runtime")?;
	Ok(HOST.get_or_init(|| host))
}

fn with_foreground<R>(f: impl FnOnce(&mut Foreground) -> R) -> R {
	let mut guard = FOREGROUND.lock().unwrap_or_else(PoisonError::into_inner);
	f(guard.get_or_insert_with(Foreground::default))
}

/// The entry point android-activity looks up in this library.
#[allow(unsafe_code)] // an exported symbol
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
	init_logging();
	info!("android_main");
	if let Err(e) = run(app) {
		error!("{e:#}");
	}
	info!("android_main done");
}

fn init_logging() {
	// android-activity forwards stderr to logcat (tag RustStdoutStderr).
	let filter = tracing_subscriber::EnvFilter::try_from_default_env()
		.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
	let _ = tracing_subscriber::fmt()
		.with_env_filter(filter)
		.with_ansi(false)
		.without_time()
		.with_writer(std::io::stderr)
		.try_init();
}

fn run(app: AndroidApp) -> anyhow::Result<()> {
	let data_dir = app.internal_data_path().context("the app has no internal storage path")?;
	install_crash_reports(&data_dir);
	jni::JavaVM::singleton()?.attach_current_thread(|env| -> jni::errors::Result<()> {
		bridge::init(env)?;
		// reqwest (HTTPS queries, tsclientlib) verifies certificates with
		// the system's trust store through rustls-platform-verifier.
		let context = bridge::context(env)?;
		rustls_platform_verifier::android::init_with_env(env, context)?;
		Ok(())
	})?;
	let host = host()?;
	static PROCESS: Once = Once::new();
	PROCESS.call_once(|| {
		// Before Slint creates the first window surface.
		egl::reserve_skia_extension_slots();
		capture::register();
		camera::register();
		watch_voice(host);
	});
	if let Err(e) = bridge::request_permissions() {
		warn!("requesting permissions: {e}");
	}

	slint::android::init(app).context("Slint Android backend")?;
	let attached = host.attach();
	voelin_ui::run(voelin_ui::RunOptions {
		data_dir: Some(data_dir),
		secrets: Some(Box::new(secrets::KeystoreSecrets)),
		engine: Some(voelin_ui::HostedEngine {
			engine: attached.engine,
			runtime: attached.runtime,
			events: attached.events,
		}),
		setting_overrides: Vec::new(),
	})
}

/// Local crash reports in `<files dir>/crash-reports`, recorded only if the
/// user opted in (off by default).
fn install_crash_reports(data_dir: &Path) {
	voelin_platform::crash::set_app_version(env!("CARGO_PKG_VERSION"));
	let enabled = voelin_store::Store::open(&data_dir.join("client.db"))
		.ok()
		.and_then(|store| store.setting::<bool>(CRASH_REPORTS_SETTING).ok().flatten())
		.unwrap_or(false);
	voelin_platform::crash::install(data_dir.join(voelin_platform::crash::DIR_NAME), enabled);
}

/// Keep the voice foreground service, and the screen-sharing service's
/// notification, in step with the voice connections and our stream.
fn watch_voice(host: &'static EngineHost) {
	let mut events = host.engine().subscribe();
	host.runtime().spawn(async move {
		loop {
			match events.recv().await {
				Ok(event) => {
					let changes = with_foreground(|f| f.update(&event));
					if let Some(notice) = changes.voice
						&& let Err(e) = bridge::set_voice_notification(notice.as_ref())
					{
						warn!("voice service: {e}");
					}
					if let Some(notice) = changes.screen
						&& let Err(e) = bridge::set_screen_notification(notice.as_ref())
					{
						warn!("screen sharing notification: {e}");
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
				Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
			}
		}
	});
}

/// "Disconnect" in the voice notification.
pub fn disconnect_voice() {
	let Some(host) = HOST.get() else {
		return;
	};
	for session in with_foreground(|f| f.voice_sessions()) {
		host.engine().send(Command::DisconnectVoice { session });
	}
}

/// "Mute" / "Unmute" in the voice notification: mute every voice session,
/// or unmute them all when all are muted.
pub fn toggle_mute() {
	let Some(host) = HOST.get() else {
		return;
	};
	let (sessions, muted) =
		with_foreground(|f| (f.voice_sessions(), f.notice().is_some_and(|n| n.muted)));
	for session in sessions {
		host.engine().send(Command::SetInputMuted { session, muted: !muted });
	}
}

/// "Deafen" / "Undeafen" in the voice notification, as Mute does.
pub fn toggle_deafen() {
	let Some(host) = HOST.get() else {
		return;
	};
	let (sessions, deafened) =
		with_foreground(|f| (f.voice_sessions(), f.notice().is_some_and(|n| n.deafened)));
	for session in sessions {
		host.engine().send(Command::SetOutputMuted { session, muted: !deafened });
	}
}
