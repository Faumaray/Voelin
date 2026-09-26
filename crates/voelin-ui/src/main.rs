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
	voelin_ui::run(voelin_ui::RunOptions::default())
}
