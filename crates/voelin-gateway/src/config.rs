//! Configuration keys and where their values come from.
//!
//! Every setting is a dotted key (`history.retention_days`), described in
//! [`KEYS`]. Its value is the first one found in: the database (set at
//! runtime, see `settings.rs`), `--set key=value` on the command line, the
//! environment (`TSGW_HISTORY_RETENTION_DAYS`), `tsgw.toml`, the built-in
//! default.
//!
//! Bootstrap keys (query login, addresses, database path) are needed before
//! the database opens; they only come from the command line, environment and
//! file, and apply at start.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use voelin_gateway_proto::{Action, PermRule};
use voelin_query::Transport;

/// Value type of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	Bool,
	/// Non-negative integer.
	Int,
	Str,
	/// A string or unset (`null`).
	OptStr,
	IntList,
	StrList,
	/// A [`PermRule`] or unset (`null`: the action's default).
	Rule,
}

impl Kind {
	pub fn name(self) -> &'static str {
		match self {
			Kind::Bool => "bool",
			Kind::Int => "int",
			Kind::Str | Kind::OptStr => "string",
			Kind::IntList => "int_list",
			Kind::StrList => "string_list",
			Kind::Rule => "rule",
		}
	}
}

/// One configuration key.
#[derive(Debug)]
pub struct KeyDef {
	pub key: &'static str,
	pub kind: Kind,
	/// Default as JSON.
	pub default: &'static str,
	pub description: &'static str,
	/// Read at start only, never from the database.
	pub bootstrap: bool,
	/// Masked in listings.
	pub secret: bool,
}

impl KeyDef {
	pub fn default_value(&self) -> Value {
		serde_json::from_str(self.default).expect("valid default")
	}

	/// Environment variable for this key.
	pub fn env_name(&self) -> String {
		format!("TSGW_{}", self.key.to_uppercase().replace('.', "_"))
	}
}

const fn boot(
	key: &'static str,
	kind: Kind,
	default: &'static str,
	description: &'static str,
) -> KeyDef {
	KeyDef { key, kind, default, description, bootstrap: true, secret: false }
}

const fn key(
	key: &'static str,
	kind: Kind,
	default: &'static str,
	description: &'static str,
) -> KeyDef {
	KeyDef { key, kind, default, description, bootstrap: false, secret: false }
}

/// Every key. Quotas and limits are off (0) or generous unless an admin sets them.
pub static KEYS: &[KeyDef] = &[
	boot(
		"gateway_id",
		Kind::OptStr,
		"null",
		"Identifies this gateway in signed login challenges; default: listen.public_url or tsgw@<query.addr>",
	),
	boot("server.voice_port", Kind::Int, "9987", "Voice port of the virtual server to serve"),
	boot(
		"query.transport",
		Kind::OptStr,
		"null",
		"\"ssh\" (TeamSpeak 3 and 6) or \"raw\" (TeamSpeak 3)",
	),
	boot("query.addr", Kind::OptStr, "null", "host:port of the ServerQuery interface"),
	boot("query.user", Kind::Str, "\"serveradmin\"", "ServerQuery login name"),
	KeyDef {
		key: "query.password",
		kind: Kind::OptStr,
		default: "null",
		description: "ServerQuery password; prefer query.password_file or TSGW_QUERY_PASSWORD",
		bootstrap: true,
		secret: true,
	},
	boot("query.password_file", Kind::OptStr, "null", "File holding the ServerQuery password"),
	boot(
		"query.allowlisted",
		Kind::Bool,
		"false",
		"The gateway's IP is on the server's query allowlist (no client-side flood limit)",
	),
	boot("listen.bind", Kind::Str, "\"0.0.0.0:7788\"", "Address the WebSocket server listens on"),
	boot(
		"listen.public_url",
		Kind::OptStr,
		"null",
		"URL clients use, e.g. wss://gw.example.org/v1",
	),
	boot(
		"history.path",
		Kind::Str,
		"\"tsgw.db\"",
		"SQLite database (history, tokens, runtime settings)",
	),
	key(
		"auth.require_server_groups",
		Kind::IntList,
		"[]",
		"If not empty, users need one of these server groups",
	),
	key("auth.deny_uids", Kind::StrList, "[]", "Unique ids that may never use the gateway"),
	key(
		"auth.min_security_level",
		Kind::Int,
		"0",
		"Minimum identity level, on top of the server's own requirement",
	),
	key("auth.token_ttl_hours", Kind::Int, "720", "Lifetime of login tokens"),
	key(
		"relay.nickname",
		Kind::Str,
		"\"Chat Relay\"",
		"Nickname of relay sessions (the channel id is appended)",
	),
	key(
		"relay.format",
		Kind::Str,
		"\"[{nick}] {text}\"",
		"How posts appear when the relay cannot take the user's nickname; {nick} and {text} are replaced",
	),
	key(
		"relay.idle_teardown_secs",
		Kind::Int,
		"120",
		"Close a relay this long after its last reader left (unless pinned)",
	),
	key(
		"relay.max_channel_relays",
		Kind::Int,
		"6",
		"Concurrent relays, each one query connection; keep below the server's per-IP limit (0: no limit)",
	),
	key(
		"relay.pinned_channels",
		Kind::IntList,
		"[]",
		"Channels relayed all the time, so their history is complete",
	),
	key(
		"history.enabled",
		Kind::Bool,
		"true",
		"Store chat messages (needed for pins, reactions and topics)",
	),
	key(
		"history.retention_days",
		Kind::Int,
		"0",
		"Delete messages older than this (0: keep forever)",
	),
	key("features.pins", Kind::Bool, "true", "Pinned messages"),
	key("features.reactions", Kind::Bool, "true", "Emoji reactions"),
	key("features.topics", Kind::Bool, "true", "Topics (threads) in chats"),
	key("features.events", Kind::Bool, "true", "Scheduled events with RSVP"),
	key("features.streams", Kind::Bool, "true", "Directory of running streams"),
	key("features.activity", Kind::Bool, "true", "Server activity feed"),
	key(
		"topics.relay_format",
		Kind::Str,
		"\"[#{topic}] {text}\"",
		"How topic posts appear in TeamSpeak; {topic} and {text} are replaced. Messages from TeamSpeak in this format join the topic",
	),
	key(
		"events.reminder_minutes",
		Kind::IntList,
		"[15, 0]",
		"Remind event subscribers this many minutes before an event starts (0: when it starts)",
	),
	key(
		"events.stream_link_minutes",
		Kind::Int,
		"60",
		"A stream by the host of a stream event links to it from this long before the start until this long after the end",
	),
	key(
		"activity.retention_days",
		Kind::Int,
		"0",
		"Delete activity entries older than this (0: keep forever)",
	),
	key(
		"limits.posts_per_window",
		Kind::Int,
		"5",
		"Chat posts per connection and window (0: no limit)",
	),
	key("limits.post_window_secs", Kind::Int, "5", "Window of limits.posts_per_window"),
	key(
		"limits.actions_per_window",
		Kind::Int,
		"30",
		"Other changes (pins, reactions, topics, events, streams) per connection and window (0: no limit)",
	),
	key("limits.action_window_secs", Kind::Int, "10", "Window of limits.actions_per_window"),
	key("quota.history_page", Kind::Int, "0", "Most messages per history page (0: no limit)"),
	key("quota.pins_per_channel", Kind::Int, "0", "Pinned messages per chat (0: no limit)"),
	key("quota.topics_per_channel", Kind::Int, "0", "Open topics per chat (0: no limit)"),
	key(
		"quota.events_per_user",
		Kind::Int,
		"0",
		"Upcoming events a user may have created (0: no limit)",
	),
	key("quota.reactions_per_message", Kind::Int, "0", "Different emoji per message (0: no limit)"),
	key("quota.reaction_bytes", Kind::Int, "64", "Longest reaction in bytes (0: no limit)"),
	key(
		"quota.message_bytes",
		Kind::Int,
		"0",
		"Longest chat post in bytes (0: no limit; TeamSpeak splits long ones)",
	),
	key("quota.streams_per_user", Kind::Int, "0", "Directory entries per user (0: no limit)"),
	key("perm.react", Kind::Rule, "null", "Who may react; default: everyone who can read the chat"),
	key(
		"perm.pin",
		Kind::Rule,
		"null",
		"Who may pin; default: moderators (b_channel_modify_name in the channel) and admins",
	),
	key(
		"perm.create_topic",
		Kind::Rule,
		"null",
		"Who may start topics; default: everyone who can post in the chat",
	),
	key(
		"perm.create_event",
		Kind::Rule,
		"null",
		"Who may schedule events; default: everyone who can post in the event's channel (server chat for server-wide events)",
	),
	key("perm.rsvp", Kind::Rule, "null", "Who may answer events; default: everyone"),
	key(
		"perm.stream",
		Kind::Rule,
		"null",
		"Who may list streams in the directory; default: everyone",
	),
	key(
		"perm.moderate",
		Kind::Rule,
		"null",
		"Who may edit or delete others' topics, events and pins; default: b_channel_modify_name in the channel, admins",
	),
	key(
		"perm.admin",
		Kind::Rule,
		"null",
		"Who may change settings and rules; default: b_virtualserver_modify_name (Server Admin)",
	),
];

pub fn key_def(key: &str) -> Option<&'static KeyDef> {
	KEYS.iter().find(|k| k.key == key)
}

/// The config key holding an action's rule.
pub fn perm_key(action: Action) -> String {
	format!("perm.{}", action.as_str())
}

/// Check a value against a key's type; returns it normalized.
pub fn validate(def: &KeyDef, value: Value) -> Result<Value> {
	let ok = match def.kind {
		Kind::Bool => value.is_boolean(),
		Kind::Int => value.is_u64(),
		Kind::Str => value.is_string(),
		Kind::OptStr => value.is_string() || value.is_null(),
		Kind::IntList => value.as_array().is_some_and(|a| a.iter().all(Value::is_u64)),
		Kind::StrList => value.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
		Kind::Rule => {
			if value.is_null() {
				true
			} else {
				serde_json::from_value::<PermRule>(value.clone())
					.map_err(|e| anyhow!("{}: invalid rule: {e}", def.key))?;
				value.is_object()
			}
		}
	};
	if !ok {
		bail!("{}: expected {}, got {value}", def.key, def.kind.name());
	}
	if def.key == "listen.bind" {
		value.as_str().unwrap_or_default().parse::<SocketAddr>().context("listen.bind")?;
	}
	if def.key == "query.transport" && !value.is_null() {
		transport(&value)?;
	}
	Ok(value)
}

fn transport(value: &Value) -> Result<Transport> {
	let transport: Transport = serde_json::from_value(value.clone())
		.context("query.transport must be \"ssh\" or \"raw\"")?;
	if transport == Transport::Http {
		bail!("query.transport = \"http\" has no events; use \"ssh\" or \"raw\"");
	}
	Ok(transport)
}

/// Parse a value given as text (environment, command line).
pub fn parse_text(def: &KeyDef, text: &str) -> Result<Value> {
	let text = text.trim();
	let value = match def.kind {
		Kind::Bool => match text.to_ascii_lowercase().as_str() {
			"true" | "1" | "yes" | "on" => Value::Bool(true),
			"false" | "0" | "no" | "off" => Value::Bool(false),
			_ => bail!("{}: expected true or false, got {text:?}", def.key),
		},
		Kind::Int => Value::from(
			text.parse::<u64>().with_context(|| format!("{}: expected a number", def.key))?,
		),
		Kind::Str | Kind::OptStr => Value::String(text.to_string()),
		Kind::IntList | Kind::StrList if !text.starts_with('[') => Value::Array(
			text.split(',')
				.map(str::trim)
				.filter(|s| !s.is_empty())
				.map(|s| match def.kind {
					Kind::IntList => s
						.parse::<u64>()
						.map(Value::from)
						.with_context(|| format!("{}: expected numbers", def.key)),
					_ => Ok(Value::String(s.to_string())),
				})
				.collect::<Result<_>>()?,
		),
		_ => serde_json::from_str(text).with_context(|| format!("{}: invalid JSON", def.key))?,
	};
	validate(def, value)
}

/// Values by key.
pub type Values = BTreeMap<String, Value>;

/// The values of one source.
pub fn file_values(text: &str) -> Result<Values> {
	let table: toml::Table = toml::from_str(text)?;
	let mut out = Values::new();
	flatten(&table, "", &mut out)?;
	Ok(out)
}

fn flatten(table: &toml::Table, prefix: &str, out: &mut Values) -> Result<()> {
	for (k, v) in table {
		let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
		if let Some(def) = key_def(&path) {
			let json = serde_json::to_value(v).context("value")?;
			out.insert(path, validate(def, json)?);
		} else if let toml::Value::Table(t) = v {
			flatten(t, &path, out)?;
		} else {
			bail!("unknown setting {path}");
		}
	}
	Ok(())
}

/// Values from `TSGW_<KEY>` variables.
pub fn env_values(lookup: impl Fn(&str) -> Option<String>) -> Result<Values> {
	let mut out = Values::new();
	for def in KEYS {
		if let Some(text) = lookup(&def.env_name()) {
			out.insert(def.key.to_string(), parse_text(def, &text)?);
		}
	}
	Ok(out)
}

/// Values from `--set key=value`.
pub fn cli_values(sets: &[String]) -> Result<Values> {
	let mut out = Values::new();
	for set in sets {
		let (k, v) =
			set.split_once('=').ok_or_else(|| anyhow!("--set needs key=value, got {set:?}"))?;
		let def = key_def(k.trim()).ok_or_else(|| anyhow!("unknown setting {k}"))?;
		out.insert(def.key.to_string(), parse_text(def, v)?);
	}
	Ok(out)
}

/// Where a value came from, lowest precedence last.
pub use voelin_gateway_proto::ConfigSource as Source;

/// The sources below the database.
#[derive(Clone, Debug, Default)]
pub struct Layers {
	/// The configuration file, for reloads.
	pub path: Option<PathBuf>,
	pub cli: Values,
	pub env: Values,
	pub file: Values,
}

impl Layers {
	/// Read the file, environment and `--set` arguments.
	pub fn load(path: &Path, sets: &[String]) -> Result<Self> {
		Ok(Self {
			path: Some(path.to_path_buf()),
			cli: cli_values(sets)?,
			env: env_values(|name| std::env::var(name).ok())?,
			file: Self::read_file(path)?,
		})
	}

	pub fn read_file(path: &Path) -> Result<Values> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("failed to read {}", path.display()))?;
		file_values(&text).with_context(|| format!("invalid {}", path.display()))
	}

	/// Value and source of a key, ignoring the database.
	pub fn resolve(&self, def: &KeyDef) -> (Value, Source) {
		for (values, source) in
			[(&self.cli, Source::Cli), (&self.env, Source::Env), (&self.file, Source::File)]
		{
			if let Some(v) = values.get(def.key) {
				return (v.clone(), source);
			}
		}
		(def.default_value(), Source::Default)
	}

	fn get<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<T> {
		let def = key_def(key).expect("known key");
		serde_json::from_value(self.resolve(def).0).with_context(|| key.to_string())
	}

	/// The bootstrap values.
	pub fn bootstrap(&self) -> Result<Bootstrap> {
		let transport = self.resolve(key_def("query.transport").unwrap()).0;
		if transport.is_null() {
			bail!("query.transport is not set");
		}
		let mut password: Option<String> = self.get("query.password")?;
		if let Some(file) = self.get::<Option<PathBuf>>("query.password_file")? {
			let pw = std::fs::read_to_string(&file)
				.with_context(|| format!("failed to read {}", file.display()))?;
			password = Some(pw.trim().to_string());
		}
		Ok(Bootstrap {
			gateway_id: self.get("gateway_id")?,
			voice_port: self.get("server.voice_port")?,
			transport: transport_of(&transport)?,
			addr: self.get::<Option<String>>("query.addr")?.context("query.addr is not set")?,
			user: self.get("query.user")?,
			password,
			allowlisted: self.get("query.allowlisted")?,
			bind: self.get::<String>("listen.bind")?.parse().context("listen.bind")?,
			public_url: self.get("listen.public_url")?,
			db_path: self.get("history.path")?,
		})
	}
}

fn transport_of(value: &Value) -> Result<Transport> {
	transport(value)
}

/// What the gateway needs before its database opens.
#[derive(Clone, Debug)]
pub struct Bootstrap {
	pub gateway_id: Option<String>,
	pub voice_port: u16,
	pub transport: Transport,
	pub addr: String,
	pub user: String,
	pub password: Option<String>,
	pub allowlisted: bool,
	pub bind: SocketAddr,
	pub public_url: Option<String>,
	pub db_path: PathBuf,
}

impl Bootstrap {
	pub fn gateway_id(&self) -> String {
		self.gateway_id
			.clone()
			.or_else(|| self.public_url.clone())
			.unwrap_or_else(|| format!("tsgw@{}", self.addr))
	}

	pub fn query_connect(&self) -> voelin_query::Connect {
		voelin_query::Connect {
			transport: self.transport,
			addr: self.addr.clone(),
			user: self.user.clone(),
			secret: self.password.clone(),
			server_port: Some(self.voice_port),
			server_id: None,
			line: voelin_query::LineOptions {
				rate_limit: if self.allowlisted {
					None
				} else {
					voelin_query::LineOptions::default().rate_limit
				},
				..Default::default()
			},
		}
	}
}

/// Settings that can change at runtime, as the gateway uses them.
#[derive(Clone, Debug, Deserialize)]
pub struct Runtime {
	pub auth: AuthSettings,
	pub relay: RelaySettings,
	pub history: HistorySettings,
	pub features: Features,
	pub topics: TopicSettings,
	pub events: EventSettings,
	pub activity: ActivitySettings,
	pub limits: Limits,
	pub quota: Quotas,
	pub perm: PermRules,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AuthSettings {
	pub require_server_groups: Vec<u64>,
	pub deny_uids: Vec<String>,
	pub min_security_level: u8,
	pub token_ttl_hours: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RelaySettings {
	pub nickname: String,
	pub format: String,
	pub idle_teardown_secs: u64,
	pub max_channel_relays: u64,
	pub pinned_channels: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HistorySettings {
	pub enabled: bool,
	pub retention_days: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Features {
	pub pins: bool,
	pub reactions: bool,
	pub topics: bool,
	pub events: bool,
	pub streams: bool,
	pub activity: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TopicSettings {
	pub relay_format: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EventSettings {
	pub reminder_minutes: Vec<u64>,
	pub stream_link_minutes: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ActivitySettings {
	pub retention_days: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Limits {
	pub posts_per_window: u64,
	pub post_window_secs: u64,
	pub actions_per_window: u64,
	pub action_window_secs: u64,
}

/// 0 means no limit.
#[derive(Clone, Debug, Deserialize)]
pub struct Quotas {
	pub history_page: u64,
	pub pins_per_channel: u64,
	pub topics_per_channel: u64,
	pub events_per_user: u64,
	pub reactions_per_message: u64,
	pub reaction_bytes: u64,
	pub message_bytes: u64,
	pub streams_per_user: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PermRules {
	pub react: Option<PermRule>,
	pub pin: Option<PermRule>,
	pub create_topic: Option<PermRule>,
	pub create_event: Option<PermRule>,
	pub rsvp: Option<PermRule>,
	pub stream: Option<PermRule>,
	pub moderate: Option<PermRule>,
	pub admin: Option<PermRule>,
}

impl PermRules {
	pub fn get(&self, action: Action) -> Option<&PermRule> {
		match action {
			Action::React => self.react.as_ref(),
			Action::Pin => self.pin.as_ref(),
			Action::CreateTopic => self.create_topic.as_ref(),
			Action::CreateEvent => self.create_event.as_ref(),
			Action::Rsvp => self.rsvp.as_ref(),
			Action::Stream => self.stream.as_ref(),
			Action::Moderate => self.moderate.as_ref(),
			Action::Admin => self.admin.as_ref(),
			Action::Unknown => None,
		}
	}
}

impl Runtime {
	/// Build from effective values of the runtime keys.
	pub fn from_values(get: impl Fn(&KeyDef) -> Value) -> Result<Self> {
		let mut root = serde_json::Map::new();
		for def in KEYS.iter().filter(|d| !d.bootstrap) {
			let (section, name) = def.key.split_once('.').expect("runtime keys have a section");
			root.entry(section)
				.or_insert_with(|| Value::Object(Default::default()))
				.as_object_mut()
				.expect("section")
				.insert(name.to_string(), get(def));
		}
		Ok(serde_json::from_value(Value::Object(root))?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_are_valid() {
		for def in KEYS {
			validate(def, def.default_value()).unwrap();
			assert!(!def.description.is_empty());
		}
		let rt = Runtime::from_values(KeyDef::default_value).unwrap();
		assert_eq!(rt.relay.max_channel_relays, 6);
		assert_eq!(rt.history.retention_days, 0);
		assert!(rt.features.pins && rt.history.enabled);
		assert_eq!(rt.events.reminder_minutes, [15, 0]);
		assert!(rt.perm.admin.is_none());
	}

	#[test]
	fn parses_example() {
		let values = file_values(include_str!("../tsgw.example.toml")).unwrap();
		let layers = Layers { file: values, ..Default::default() };
		let boot = layers.bootstrap().unwrap();
		assert_eq!(boot.transport, Transport::Ssh);
		assert_eq!(boot.voice_port, 9987);
		assert_eq!(layers.resolve(key_def("relay.format").unwrap()).0, "[{nick}] {text}");
	}

	#[test]
	fn minimal_config_uses_defaults() {
		let file =
			file_values("[server]\n[query]\ntransport = \"raw\"\naddr = \"127.0.0.1:10011\"\n")
				.unwrap();
		let layers = Layers { file, ..Default::default() };
		let boot = layers.bootstrap().unwrap();
		assert_eq!(boot.bind.port(), 7788);
		assert_eq!(boot.gateway_id(), "tsgw@127.0.0.1:10011");
		assert_eq!(boot.db_path, PathBuf::from("tsgw.db"));
		let (v, source) = layers.resolve(key_def("relay.max_channel_relays").unwrap());
		assert_eq!((v, source), (Value::from(6), Source::Default));
	}

	#[test]
	fn rejects_bad_values() {
		assert!(file_values("[relay]\nunknown = 1\n").is_err());
		assert!(file_values("[history]\nretention_days = \"x\"\n").is_err());
		assert!(file_values("[query]\ntransport = \"http\"\n").is_err());
		assert!(file_values("[listen]\nbind = \"nope\"\n").is_err());
		assert!(file_values("[perm]\npin = { server_groups = [\"a\"] }\n").is_err());
		let rule =
			file_values("[perm]\npin = { server_groups = [6], channel_groups = [5] }\n").unwrap();
		assert_eq!(rule["perm.pin"]["server_groups"][0], 6);
	}

	#[test]
	fn env_and_cli_text() {
		let env = env_values(|name| match name {
			"TSGW_HISTORY_RETENTION_DAYS" => Some("7".into()),
			"TSGW_RELAY_PINNED_CHANNELS" => Some("1, 2".into()),
			"TSGW_FEATURES_PINS" => Some("off".into()),
			"TSGW_PERM_ADMIN" => Some(r#"{"server_groups":[6]}"#.into()),
			"TSGW_QUERY_PASSWORD" => Some("pw".into()),
			_ => None,
		})
		.unwrap();
		assert_eq!(env["history.retention_days"], 7);
		assert_eq!(env["relay.pinned_channels"], serde_json::json!([1, 2]));
		assert_eq!(env["features.pins"], false);
		assert_eq!(env["perm.admin"]["server_groups"], serde_json::json!([6]));
		assert_eq!(env["query.password"], "pw");
		let cli =
			cli_values(&["auth.deny_uids=[\"a\",\"b\"]".into(), "relay.nickname= Bot ".into()])
				.unwrap();
		assert_eq!(cli["auth.deny_uids"], serde_json::json!(["a", "b"]));
		assert_eq!(cli["relay.nickname"], "Bot");
		assert!(cli_values(&["nope=1".into()]).is_err());
		assert!(cli_values(&["history.retention_days=-1".into()]).is_err());
	}
}
