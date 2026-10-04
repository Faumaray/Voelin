//! Desktop client.

// Release builds on Windows are GUI programs (no console window); debug
// builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use tracing_subscriber::{EnvFilter, Layer, fmt, prelude::*};
use voelin_platform::logs;

/// What the log file gets without `VOELIN_LOG`: our crates' information,
/// everyone's warnings.
const FILE_FILTER: &str = "warn,voelin=info,voelin_ui=info,voelin_core=info,voelin_media=info,\
	voelin_stream=info,voelin_audio=info,voelin_store=info,voelin_myts=info,voelin_observer=info,\
	voelin_query=info,voelin_gateway_proto=info,voelin_platform=info,tsclientlib=info";

fn main() -> Result<()> {
	// The terminal (RUST_LOG, warnings by default) and the log file in the
	// state directory (VOELIN_LOG, `FILE_FILTER` by default): release builds
	// on Windows have no terminal.
	let file = logs::LogFile::open(logs::default_dir());
	let console = fmt::layer()
		.with_writer(std::io::stderr)
		.with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")));
	let to_file = file.as_ref().ok().map(|log| {
		let log = log.clone();
		fmt::layer().with_ansi(false).with_writer(move || log.clone()).with_filter(
			EnvFilter::try_from_env("VOELIN_LOG").unwrap_or_else(|_| EnvFilter::new(FILE_FILTER)),
		)
	});
	tracing_subscriber::registry().with(console).with(to_file).init();
	match &file {
		Ok(log) => tracing::info!(
			version = env!("CARGO_PKG_VERSION"),
			os = std::env::consts::OS,
			log = %log.path().display(),
			"Voelin starting"
		),
		Err(error) => tracing::warn!(%error, "no log file this time"),
	}
	let options = voelin_ui::RunOptions {
		setting_overrides: setting_overrides(std::env::args().skip(1)),
		..Default::default()
	};
	voelin_ui::run(options)
}

/// `--set key=value` (or `--set=key=value`), repeatable: settings for this
/// run, below those changed in the app.
fn setting_overrides(mut args: impl Iterator<Item = String>) -> Vec<String> {
	let mut overrides = Vec::new();
	while let Some(arg) = args.next() {
		match arg.strip_prefix("--set") {
			Some("") => overrides.extend(args.next()),
			Some(rest) if rest.starts_with('=') => overrides.push(rest[1..].to_owned()),
			_ => eprintln!("ignoring argument {arg:?}; usage: voelin [--set key=value]..."),
		}
	}
	overrides
}
