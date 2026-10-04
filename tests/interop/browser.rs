//! The Node.js side of the browser interop tests (headless Chromium through
//! `tests/interop/browser-peer.cjs`), shared by the tests of several crates
//! with `#[path = ".../tests/interop/browser.rs"] mod browser;`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

/// The tests run only with `VOELIN_INTEROP=1` (see `tests/interop/README.md`).
pub fn enabled() -> bool {
	std::env::var("VOELIN_INTEROP").is_ok_and(|v| v == "1")
}

/// Headless Chromium, driven over stdin/stdout.
pub struct Browser {
	_child: Child,
	stdin: ChildStdin,
	stdout: Lines<BufReader<ChildStdout>>,
}

impl Browser {
	pub async fn start() -> Self {
		Self::launch(false).await
	}

	/// A browser that hides its host addresses behind mDNS names
	/// (`<uuid>.local`), as Chromium does without camera or microphone
	/// permission.
	#[allow(dead_code, reason = "not every test uses it")]
	pub async fn start_with_mdns() -> Self {
		Self::launch(true).await
	}

	async fn launch(mdns: bool) -> Self {
		let script = Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("../../tests/interop/browser-peer.cjs")
			.canonicalize()
			.expect("tests/interop/browser-peer.cjs");
		let node = std::env::var("VOELIN_NODE").unwrap_or_else(|_| "node".into());
		let mut command = Command::new(node);
		command
			.arg(script)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.env("VOELIN_BROWSER_MDNS", if mdns { "1" } else { "0" })
			.kill_on_drop(true);
		// Playwright is usually installed globally.
		if std::env::var_os("NODE_PATH").is_none()
			&& let Ok(out) = std::process::Command::new("npm").args(["root", "-g"]).output()
		{
			command.env("NODE_PATH", String::from_utf8_lossy(&out.stdout).trim());
		}
		let mut child = command.spawn().expect("cannot start node (set VOELIN_NODE)");
		let stdin = child.stdin.take().unwrap();
		let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
		let mut browser = Self { _child: child, stdin, stdout };
		let ready = browser.read().await;
		eprintln!("browser: {}", ready["userAgent"]);
		browser
	}

	async fn read(&mut self) -> Value {
		let line = timeout(Duration::from_secs(60), self.stdout.next_line())
			.await
			.expect("browser did not answer")
			.unwrap()
			.expect("browser exited");
		let value: Value = serde_json::from_str(&line).unwrap();
		if let Some(error) = value.get("error") {
			panic!("browser: {error}");
		}
		value
	}

	pub async fn call(&mut self, command: Value) -> Value {
		let mut line = command.to_string();
		line.push('\n');
		self.stdin.write_all(line.as_bytes()).await.unwrap();
		self.read().await
	}

	pub async fn quit(mut self) {
		let _ = self.call(json!({ "op": "quit" })).await;
	}
}
