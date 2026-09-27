//! Runtime settings: values stored in the database win over the command
//! line, environment, file and defaults ([`crate::config::Layers`]).
//!
//! Every change (set, reset, reload) builds a new [`Runtime`] and publishes
//! it on a watch channel; subsystems read the current value on use or wait
//! for changes, so nothing needs a restart.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use tokio::sync::watch;
use tracing::{info, warn};
use voelin_gateway_proto::{Action, ConfigEntry, PermRuleInfo};

use crate::config::{KEYS, KeyDef, Layers, Runtime, Source, Values, key_def, perm_key, validate};
use crate::db::Db;

/// What a reload found.
#[derive(Debug, Default, PartialEq)]
pub struct ReloadReport {
	/// Keys whose effective value changed.
	pub changed: Vec<String>,
	/// Keys where the file has a different value than the database, which wins.
	pub overridden: Vec<String>,
	/// Bootstrap keys changed in the file; they apply after a restart.
	pub restart_needed: Vec<String>,
}

pub struct Settings {
	layers: Mutex<Layers>,
	/// Values set at runtime, mirrored from the database.
	db_values: Mutex<Values>,
	tx: watch::Sender<Arc<Runtime>>,
}

impl Settings {
	/// Combine the sources with the values stored in `db`. Invalid stored
	/// values (e.g. from an older version) are ignored with a warning.
	pub fn new(layers: Layers, db: &Db) -> Result<Self> {
		let mut db_values = Values::new();
		for (key, value) in db.config_values()? {
			match key_def(&key).filter(|d| !d.bootstrap).map(|d| validate(d, value)) {
				Some(Ok(v)) => {
					db_values.insert(key, v);
				}
				Some(Err(error)) => warn!(%key, %error, "ignoring stored setting"),
				None => warn!(%key, "ignoring stored setting of unknown key"),
			}
		}
		let runtime = build(&layers, &db_values)?;
		Ok(Self {
			layers: Mutex::new(layers),
			db_values: Mutex::new(db_values),
			tx: watch::channel(Arc::new(runtime)).0,
		})
	}

	/// The configuration file, if any.
	pub fn path(&self) -> Option<std::path::PathBuf> {
		self.layers.lock().unwrap().path.clone()
	}

	pub fn current(&self) -> Arc<Runtime> {
		self.tx.borrow().clone()
	}

	/// Changes as they happen.
	pub fn subscribe(&self) -> watch::Receiver<Arc<Runtime>> {
		self.tx.subscribe()
	}

	fn entry_of(&self, def: &KeyDef, layers: &Layers, db_values: &Values) -> ConfigEntry {
		let (value, source) = resolve(def, layers, db_values);
		let mask = |v: Value| if def.secret && !v.is_null() { Value::from("***") } else { v };
		ConfigEntry {
			key: def.key.to_string(),
			value: mask(value),
			source,
			default: def.default_value(),
			value_type: def.kind.name().to_string(),
			description: def.description.to_string(),
			bootstrap: def.bootstrap,
		}
	}

	pub fn entries(&self) -> Vec<ConfigEntry> {
		let layers = self.layers.lock().unwrap();
		let db_values = self.db_values.lock().unwrap();
		KEYS.iter().map(|d| self.entry_of(d, &layers, &db_values)).collect()
	}

	pub fn entry(&self, key: &str) -> Option<ConfigEntry> {
		let def = key_def(key)?;
		Some(self.entry_of(def, &self.layers.lock().unwrap(), &self.db_values.lock().unwrap()))
	}

	/// Store a value at runtime; it applies at once.
	pub fn set(&self, db: &Db, key: &str, value: Value, by: Option<&str>) -> Result<ConfigEntry> {
		let def = key_def(key).ok_or_else(|| anyhow!("unknown setting {key}"))?;
		if def.bootstrap {
			bail!("{key} is read at start only; change it in tsgw.toml, the environment or --set");
		}
		let value = validate(def, value)?;
		let mut db_values = self.db_values.lock().unwrap();
		let mut candidate = db_values.clone();
		candidate.insert(key.to_string(), value.clone());
		let layers = self.layers.lock().unwrap();
		let runtime = build(&layers, &candidate)?;
		db.set_config(key, &value, by)?;
		*db_values = candidate;
		info!(key, %value, by, "setting changed");
		self.tx.send_replace(Arc::new(runtime));
		Ok(self.entry_of(def, &layers, &db_values))
	}

	/// Drop the runtime value; the key falls back to the lower sources.
	pub fn reset(&self, db: &Db, key: &str, by: Option<&str>) -> Result<ConfigEntry> {
		let def = key_def(key).ok_or_else(|| anyhow!("unknown setting {key}"))?;
		let mut db_values = self.db_values.lock().unwrap();
		let layers = self.layers.lock().unwrap();
		if db_values.remove(key).is_some() {
			db.delete_config(key)?;
			info!(key, by, "setting reset");
			self.tx.send_replace(Arc::new(build(&layers, &db_values)?));
		}
		Ok(self.entry_of(def, &layers, &db_values))
	}

	/// Read the file again. Values stored at runtime keep winning; keys where
	/// the file now differs from them are reported (and logged).
	pub fn reload(&self) -> Result<ReloadReport> {
		let mut layers = self.layers.lock().unwrap();
		let Some(path) = layers.path.clone() else { return Ok(ReloadReport::default()) };
		let file = Layers::read_file(&path)?;
		let db_values = self.db_values.lock().unwrap();
		let new_layers = Layers { file, ..layers.clone() };
		let runtime = build(&new_layers, &db_values)?;
		let mut report = ReloadReport::default();
		for def in KEYS {
			let old = resolve(def, &layers, &db_values).0;
			let new = resolve(def, &new_layers, &db_values).0;
			let file_value = new_layers.file.get(def.key);
			if def.bootstrap {
				if layers.file.get(def.key) != file_value {
					report.restart_needed.push(def.key.to_string());
				}
			} else if old != new {
				report.changed.push(def.key.to_string());
			}
			if let (Some(stored), Some(file_value)) = (db_values.get(def.key), file_value)
				&& stored != file_value
			{
				report.overridden.push(def.key.to_string());
				info!(
					key = def.key,
					file = %file_value,
					database = %stored,
					"file differs from the value set at runtime, which wins (ConfigReset uses the file)"
				);
			}
		}
		for key in &report.restart_needed {
			warn!(%key, "changed in the file; applies after a restart");
		}
		*layers = new_layers;
		info!(path = %path.display(), changed = ?report.changed, "configuration reloaded");
		self.tx.send_replace(Arc::new(runtime));
		Ok(report)
	}

	/// Rules of every action, with where they come from.
	pub fn perm_rules(&self) -> Vec<PermRuleInfo> {
		let layers = self.layers.lock().unwrap();
		let db_values = self.db_values.lock().unwrap();
		Action::ALL
			.into_iter()
			.map(|action| {
				let def = key_def(&perm_key(action)).expect("perm key");
				let (value, source) = resolve(def, &layers, &db_values);
				PermRuleInfo {
					action,
					rule: serde_json::from_value(value).ok().flatten(),
					source,
					default: def.description.split("default: ").nth(1).unwrap_or("").to_string(),
				}
			})
			.collect()
	}
}

fn resolve(def: &KeyDef, layers: &Layers, db_values: &Values) -> (Value, Source) {
	match db_values.get(def.key) {
		Some(v) if !def.bootstrap => (v.clone(), Source::Db),
		_ => layers.resolve(def),
	}
}

fn build(layers: &Layers, db_values: &Values) -> Result<Runtime> {
	Runtime::from_values(|def| resolve(def, layers, db_values).0)
}

#[cfg(test)]
mod tests {
	use std::io::Write;

	use serde_json::json;

	use super::*;
	use crate::config::{cli_values, env_values, file_values};

	fn layers(file: &str, env: &[(&str, &str)], cli: &[&str]) -> Layers {
		let env: Vec<(String, String)> =
			env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
		Layers {
			path: None,
			cli: cli_values(&cli.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap(),
			env: env_values(|name| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
				.unwrap(),
			file: file_values(file).unwrap(),
		}
	}

	#[test]
	fn precedence_db_cli_env_file_default() {
		let db = Db::in_memory().unwrap();
		let l = layers(
			"[history]\nretention_days = 30\n[relay]\nidle_teardown_secs = 60\nnickname = \"File\"\nformat = \"<{nick}> {text}\"\n",
			&[("TSGW_HISTORY_RETENTION_DAYS", "20"), ("TSGW_RELAY_NICKNAME", "Env")],
			&["relay.nickname=Cli"],
		);
		let s = Settings::new(l, &db).unwrap();
		let e = |k: &str| {
			let e = s.entry(k).unwrap();
			(e.value, e.source)
		};
		assert_eq!(e("relay.max_channel_relays"), (json!(6), Source::Default));
		assert_eq!(e("relay.format"), (json!("<{nick}> {text}"), Source::File));
		assert_eq!(e("history.retention_days"), (json!(20), Source::Env));
		assert_eq!(e("relay.nickname"), (json!("Cli"), Source::Cli));
		s.set(&db, "relay.nickname", json!("Db"), Some("admin")).unwrap();
		s.set(&db, "history.retention_days", json!(0), None).unwrap();
		assert_eq!(e("relay.nickname"), (json!("Db"), Source::Db));
		assert_eq!(s.current().history.retention_days, 0);
		// Stored values survive a restart and still win.
		let s2 = Settings::new(
			layers("[relay]\nnickname = \"File\"\n", &[], &["relay.nickname=Cli"]),
			&db,
		)
		.unwrap();
		assert_eq!(s2.current().relay.nickname, "Db");
		// Reset falls back to the next source.
		let entry = s.reset(&db, "relay.nickname", None).unwrap();
		assert_eq!((entry.value, entry.source), (json!("Cli"), Source::Cli));
		assert_eq!(s.current().relay.nickname, "Cli");
	}

	#[test]
	fn changes_apply_live() {
		let db = Db::in_memory().unwrap();
		let s = Settings::new(layers("", &[], &[]), &db).unwrap();
		let mut rx = s.subscribe();
		assert!(s.current().features.pins);
		s.set(&db, "features.pins", json!(false), None).unwrap();
		assert!(rx.has_changed().unwrap());
		assert!(!rx.borrow_and_update().features.pins);
		s.set(&db, "perm.pin", json!({"server_groups": [6]}), None).unwrap();
		assert_eq!(rx.borrow_and_update().perm.pin.as_ref().unwrap().server_groups, [6]);
		let rules = s.perm_rules();
		let pin = rules.iter().find(|r| r.action == Action::Pin).unwrap();
		assert_eq!(
			(pin.source, pin.rule.as_ref().unwrap().server_groups.clone()),
			(Source::Db, vec![6])
		);
		assert!(pin.default.contains("moderators"));
	}

	#[test]
	fn rejects_invalid_and_bootstrap() {
		let db = Db::in_memory().unwrap();
		let s = Settings::new(layers("", &[], &[]), &db).unwrap();
		assert!(s.set(&db, "history.retention_days", json!("x"), None).is_err());
		assert!(s.set(&db, "history.retention_days", json!(-1), None).is_err());
		assert!(s.set(&db, "auth.min_security_level", json!(300), None).is_err());
		assert!(s.set(&db, "query.addr", json!("1.2.3.4:1"), None).is_err());
		assert!(s.set(&db, "no.such", json!(1), None).is_err());
		assert!(db.config_values().unwrap().is_empty());
		// Secrets are masked.
		let s = Settings::new(layers("[query]\npassword = \"pw\"\n", &[], &[]), &db).unwrap();
		assert_eq!(s.entry("query.password").unwrap().value, "***");
		assert!(s.entry("query.password").unwrap().bootstrap);
	}

	#[test]
	fn reload_keeps_db_values() {
		let dir = std::env::temp_dir().join(format!("tsgw-reload-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("tsgw.toml");
		let write = |text: &str| {
			let mut f = std::fs::File::create(&path).unwrap();
			f.write_all(text.as_bytes()).unwrap();
		};
		write("[query]\naddr = \"a:1\"\n[relay]\nidle_teardown_secs = 60\nnickname = \"File\"\n");
		let db = Db::in_memory().unwrap();
		let s = Settings::new(Layers::load(&path, &[]).unwrap(), &db).unwrap();
		s.set(&db, "relay.nickname", json!("Db"), None).unwrap();
		write("[query]\naddr = \"b:2\"\n[relay]\nidle_teardown_secs = 90\nnickname = \"File2\"\n");
		let report = s.reload().unwrap();
		assert_eq!(report.changed, ["relay.idle_teardown_secs"]);
		assert_eq!(report.overridden, ["relay.nickname"]);
		assert_eq!(report.restart_needed, ["query.addr"]);
		assert_eq!(s.current().relay.idle_teardown_secs, 90);
		assert_eq!(s.current().relay.nickname, "Db");
		s.reset(&db, "relay.nickname", None).unwrap();
		assert_eq!(s.current().relay.nickname, "File2");
		// A broken file changes nothing.
		write("[relay]\nidle_teardown_secs = \"x\"\n");
		assert!(s.reload().is_err());
		assert_eq!(s.current().relay.idle_teardown_secs, 90);
		std::fs::remove_dir_all(dir).unwrap();
	}
}
