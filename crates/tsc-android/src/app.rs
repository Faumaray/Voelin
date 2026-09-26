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
use tsc_core::Command;

use crate::foreground::Foreground;
use crate::host::EngineHost;
use crate::{bridge, capture, secrets};

/// Store setting (`bool`): the user's opt-in to local crash reports.
pub const CRASH_REPORTS_SETTING: &str = "crash_reports";

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
		capture::register();
		watch_voice(host);
	});
	if let Err(e) = bridge::request_permissions() {
		warn!("requesting permissions: {e}");
	}

	slint::android::init(app).context("Slint Android backend")?;
	let attached = host.attach();
	tsc_ui::run(tsc_ui::RunOptions {
		data_dir: Some(data_dir),
		secrets: Some(Box::new(secrets::KeystoreSecrets)),
		engine: Some(tsc_ui::HostedEngine {
			engine: attached.engine,
			runtime: attached.runtime,
			events: attached.events,
		}),
	})
}

/// Local crash reports in `<files dir>/crash-reports`, recorded only if the
/// user opted in (off by default).
fn install_crash_reports(data_dir: &Path) {
	tsc_platform::crash::set_app_version(env!("CARGO_PKG_VERSION"));
	let enabled = tsc_store::Store::open(&data_dir.join("client.db"))
		.ok()
		.and_then(|store| store.setting::<bool>(CRASH_REPORTS_SETTING).ok().flatten())
		.unwrap_or(false);
	tsc_platform::crash::install(data_dir.join(tsc_platform::crash::DIR_NAME), enabled);
}

/// Keep the voice foreground service in step with the voice connections.
fn watch_voice(host: &'static EngineHost) {
	let mut events = host.engine().subscribe();
	host.runtime().spawn(async move {
		loop {
			match events.recv().await {
				Ok(event) => {
					if let Some(text) = with_foreground(|f| f.update(&event))
						&& let Err(e) = bridge::set_voice_notification(text.as_deref())
					{
						warn!("voice service: {e}");
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
