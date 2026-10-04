//! Desktop client.

// Release builds on Windows are GUI programs (no console window); debug
// builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
		)
		.init();
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
