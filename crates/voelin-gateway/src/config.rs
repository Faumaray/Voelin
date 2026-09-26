//! `tsgw.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use voelin_query::Transport;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	/// Identifies this gateway in signed challenges. Defaults to `public_url`
	/// or the query address.
	#[serde(default)]
	pub gateway_id: Option<String>,
	pub server: ServerConfig,
	pub query: QueryConfig,
	#[serde(default)]
	pub listen: ListenConfig,
	#[serde(default)]
	pub auth: AuthConfig,
	#[serde(default)]
	pub relay: RelayConfig,
	#[serde(default)]
	pub history: HistoryConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
	/// Voice port of the virtual server to serve.
	#[serde(default = "default_voice_port")]
	pub voice_port: u16,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryConfig {
	/// `raw` (TeamSpeak 3) or `ssh`. HTTP has no events and cannot relay chat.
	pub transport: Transport,
	/// host:port of the query interface.
	pub addr: String,
	#[serde(default = "default_user")]
	pub user: String,
	/// Password; prefer `password_file` or the `TSGW_QUERY_PASSWORD` variable.
	#[serde(default)]
	pub password: Option<String>,
	#[serde(default)]
	pub password_file: Option<PathBuf>,
	/// The gateway's IP is on the server's query allowlist, so the client-side
	/// flood limit (10 commands per 3 s) is not needed.
	#[serde(default)]
	pub allowlisted: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
	#[serde(default = "default_bind")]
	pub bind: SocketAddr,
	/// URL clients use, e.g. `wss://gw.example.org/v1`.
	#[serde(default)]
	pub public_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
	/// If not empty, users need one of these server groups.
	#[serde(default)]
	pub require_server_groups: Vec<u64>,
	/// Unique ids that may never use the gateway.
	#[serde(default)]
	pub deny_uids: Vec<String>,
	/// Minimum identity level, on top of the server's own requirement.
	#[serde(default)]
	pub min_security_level: u8,
	#[serde(default = "default_token_ttl_hours")]
	pub token_ttl_hours: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
	#[serde(default = "default_relay_nickname")]
	pub nickname: String,
	/// How posts appear in the channel; `{nick}` and `{text}` are replaced.
	#[serde(default = "default_relay_format")]
	pub format: String,
	/// Close a relay this long after its last reader left (unless pinned).
	#[serde(default = "default_idle_teardown")]
	pub idle_teardown_secs: u64,
	/// Upper bound on concurrent relays (each is one query connection).
	#[serde(default = "default_max_relays")]
	pub max_channel_relays: usize,
	/// Always relayed, so their history is complete.
	#[serde(default)]
	pub pinned_channels: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
	#[serde(default = "default_true")]
	pub enabled: bool,
	#[serde(default = "default_db_path")]
	pub path: PathBuf,
	#[serde(default = "default_retention_days")]
	pub retention_days: u64,
}

fn default_voice_port() -> u16 {
	9987
}
fn default_user() -> String {
	"serveradmin".into()
}
fn default_bind() -> SocketAddr {
	"0.0.0.0:7788".parse().unwrap()
}
fn default_token_ttl_hours() -> u64 {
	24 * 30
}
fn default_relay_nickname() -> String {
	"Chat Relay".into()
}
fn default_relay_format() -> String {
	"[{nick}] {text}".into()
}
fn default_idle_teardown() -> u64 {
	120
}
fn default_max_relays() -> usize {
	6
}
fn default_true() -> bool {
	true
}
fn default_db_path() -> PathBuf {
	"tsgw.db".into()
}
fn default_retention_days() -> u64 {
	30
}

impl Default for ListenConfig {
	fn default() -> Self {
		Self { bind: default_bind(), public_url: None }
	}
}

impl Default for RelayConfig {
	fn default() -> Self {
		Self {
			nickname: default_relay_nickname(),
			format: default_relay_format(),
			idle_teardown_secs: default_idle_teardown(),
			max_channel_relays: default_max_relays(),
			pinned_channels: Vec::new(),
		}
	}
}

impl Default for HistoryConfig {
	fn default() -> Self {
		Self { enabled: true, path: default_db_path(), retention_days: default_retention_days() }
	}
}

impl Config {
	pub fn load(path: &Path) -> Result<Self> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("failed to read {}", path.display()))?;
		let mut config: Config =
			toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
		config.resolve_password()?;
		if config.query.transport == Transport::Http {
			bail!("query.transport = \"http\" has no events; use \"ssh\" or \"raw\"");
		}
		Ok(config)
	}

	fn resolve_password(&mut self) -> Result<()> {
		if let Some(file) = &self.query.password_file {
			let pw = std::fs::read_to_string(file)
				.with_context(|| format!("failed to read {}", file.display()))?;
			self.query.password = Some(pw.trim().to_string());
		} else if let Ok(pw) = std::env::var("TSGW_QUERY_PASSWORD") {
			self.query.password = Some(pw);
		}
		Ok(())
	}

	pub fn gateway_id(&self) -> String {
		self.gateway_id
			.clone()
			.or_else(|| self.listen.public_url.clone())
			.unwrap_or_else(|| format!("tsgw@{}", self.query.addr))
	}

	pub fn query_connect(&self) -> voelin_query::Connect {
		voelin_query::Connect {
			transport: self.query.transport,
			addr: self.query.addr.clone(),
			user: self.query.user.clone(),
			secret: self.query.password.clone(),
			server_port: Some(self.server.voice_port),
			server_id: None,
			line: voelin_query::LineOptions {
				rate_limit: if self.query.allowlisted {
					None
				} else {
					voelin_query::LineOptions::default().rate_limit
				},
				..Default::default()
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_example() {
		let text = include_str!("../tsgw.example.toml");
		let config: Config = toml::from_str(text).unwrap();
		assert_eq!(config.query.transport, Transport::Ssh);
		assert_eq!(config.relay.format, "[{nick}] {text}");
		assert_eq!(config.server.voice_port, 9987);
	}

	#[test]
	fn minimal_config_uses_defaults() {
		let config: Config =
			toml::from_str("[server]\n[query]\ntransport = \"raw\"\naddr = \"127.0.0.1:10011\"\n")
				.unwrap();
		assert_eq!(config.listen.bind.port(), 7788);
		assert_eq!(config.relay.max_channel_relays, 6);
		assert!(config.history.enabled);
		assert_eq!(config.gateway_id(), "tsgw@127.0.0.1:10011");
	}
}
