//! Settings: typed keys with defaults and validation, values from several
//! layers, change notification, and persistence in the client database.
//!
//! A setting is a [`Key`]: its name, value type, default and validation.
//! Keys are `static`s ([`STREAM_FPS`], …); a [`Settings`] registers a key the
//! first time it is used (the built-in keys when it is created), and
//! [`Settings::keys`] lists the registered keys for a settings page.
//!
//! The effective value of a key comes from the first layer that has a valid
//! one:
//!
//! 1. **runtime**: set while the app runs ([`Settings::set`],
//!    [`crate::Command::SetSetting`]); stored in the database, so it is the
//!    same after a restart. [`Settings::reset`] removes it.
//! 2. **override**: command line (`--set key=value`) or environment
//!    (`VOELIN_SETTING_STREAM__FPS=90` for `stream.fps`).
//! 3. **config**: a config file ([`Settings::load_config_file`]).
//! 4. the key's **default**.
//!
//! Overrides and config files never write to the database: they apply while
//! no runtime value exists.
//!
//! Reads are cheap: the current value of every key is kept in memory, typed
//! and behind an `Arc` ([`Settings::get_arc`]); a [`SettingWatch`] reads it
//! without looking the key up. Writes update the memory at once, notify, and
//! go to SQLite on a writer thread, batched ([`Settings::flush`] waits for
//! them).
//!
//! Changes are announced to [`Settings::subscribe`] (every set and reset),
//! to the [`SettingWatch`]es of the key (when the effective value changed),
//! and by the engine as [`crate::Event::SettingChanged`].
//!
//! Stored values are versioned: [`Settings::open`] runs the migrations the
//! database has not seen yet (see `MIGRATIONS`). The first one copies the
//! stream choices and the crash report opt-in out of the UI's `ui` blob; the
//! old blobs (`ui`, `audio`, `client_playback`) stay and remain keys.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};
use voelin_audio::AudioSettings;
use voelin_store::Store;
use voelin_stream::{LayerId, LayerSpec};

/// A value of any key, typed (`Arc<T>` of the key's `T`).
type Stored = Arc<dyn Any + Send + Sync>;

/// Where the effective value of a key comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
	/// Set while running; stored in the database.
	Runtime,
	/// Command line or environment.
	Override,
	/// A config file.
	Config,
	Default,
}

/// What a key holds, for a settings page that shows every key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	Bool,
	/// A whole number of at least `min`, without a maximum.
	UInt {
		min: u64,
	},
	/// One of these strings.
	Choice(&'static [&'static str]),
	/// Any string; these are suggestions.
	Text {
		suggestions: &'static [&'static str],
	},
	/// An ordered list of distinct strings from these.
	List(&'static [&'static str]),
	/// Structured JSON (objects, lists of objects).
	Json,
}

/// A setting: its name, value type `T`, default and validation.
pub struct Key<T> {
	name: &'static str,
	doc: &'static str,
	kind: Kind,
	default: fn() -> T,
	validate: fn(&T) -> Result<(), String>,
}

fn accept<T>(_: &T) -> Result<(), String> {
	Ok(())
}

impl<T> Key<T> {
	/// A key whose every `T` is valid.
	pub const fn new(
		name: &'static str,
		kind: Kind,
		doc: &'static str,
		default: fn() -> T,
	) -> Self {
		Self { name, doc, kind, default, validate: accept::<T> }
	}

	/// The same key with a validation (errors are shown to the user).
	pub const fn validated(self, validate: fn(&T) -> Result<(), String>) -> Self {
		Self { validate, ..self }
	}

	pub const fn name(&self) -> &'static str {
		self.name
	}

	pub fn default_value(&self) -> T {
		(self.default)()
	}

	pub fn validate(&self, value: &T) -> Result<(), String> {
		(self.validate)(value)
	}
}

impl<T> fmt::Debug for Key<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Key").field("name", &self.name).field("kind", &self.kind).finish()
	}
}

/// A [`Key`] of any type, as the registry holds it.
pub trait Setting: Send + Sync + 'static {
	fn name(&self) -> &'static str;
	/// One sentence for a settings page.
	fn doc(&self) -> &'static str;
	fn kind(&self) -> Kind;
	fn default_json(&self) -> Value;
	/// Check a JSON value; the typed value and its normalized JSON.
	fn parse_json(&self, value: &Value) -> Result<(Stored, Value), String>;
	#[doc(hidden)]
	fn default_stored(&self) -> Stored;
	#[doc(hidden)]
	fn value_type(&self) -> TypeId;
}

impl<T> Setting for Key<T>
where
	T: Serialize + DeserializeOwned + Send + Sync + 'static,
{
	fn name(&self) -> &'static str {
		self.name
	}

	fn doc(&self) -> &'static str {
		self.doc
	}

	fn kind(&self) -> Kind {
		self.kind
	}

	fn default_json(&self) -> Value {
		serde_json::to_value(self.default_value()).unwrap_or(Value::Null)
	}

	fn parse_json(&self, value: &Value) -> Result<(Stored, Value), String> {
		let typed: T = T::deserialize(value).map_err(|e| e.to_string())?;
		self.validate(&typed)?;
		let json = serde_json::to_value(&typed).map_err(|e| e.to_string())?;
		Ok((Arc::new(typed), json))
	}

	fn default_stored(&self) -> Stored {
		Arc::new(self.default_value())
	}

	fn value_type(&self) -> TypeId {
		TypeId::of::<T>()
	}
}

/// Why a setting could not be read or changed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
	#[error("unknown setting {0}")]
	UnknownKey(String),
	#[error("invalid value for {key}: {message}")]
	Invalid { key: String, message: String },
	#[error("settings database: {0}")]
	Store(String),
	#[error("config file {path}: {message}")]
	Config { path: String, message: String },
}

impl From<voelin_store::Error> for SettingsError {
	fn from(e: voelin_store::Error) -> Self {
		Self::Store(e.to_string())
	}
}

/// A set or reset of a key ([`Settings::subscribe`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingChange {
	pub key: String,
	/// Where the key's value now comes from (`None`: not registered).
	pub source: Option<Source>,
}

/// The values of one key in each layer, as JSON; kept for keys that are not
/// registered yet too.
#[derive(Clone, Debug, Default)]
struct Layers {
	runtime: Option<Value>,
	overrides: Option<Value>,
	config: Option<Value>,
}

/// A registered key and its effective value.
struct Slot {
	key: &'static dyn Setting,
	source: Source,
	json: Value,
	value: watch::Sender<Stored>,
}

#[derive(Default)]
struct State {
	layers: HashMap<String, Layers>,
	slots: HashMap<&'static str, Slot>,
}

impl State {
	/// The effective value of `key` from `layers`; invalid layers are skipped.
	fn resolve(key: &'static dyn Setting, layers: Option<&Layers>) -> (Stored, Value, Source) {
		if let Some(layers) = layers {
			for (source, value) in [
				(Source::Runtime, &layers.runtime),
				(Source::Override, &layers.overrides),
				(Source::Config, &layers.config),
			] {
				let Some(value) = value else { continue };
				match key.parse_json(value) {
					Ok((stored, json)) => return (stored, json, source),
					Err(e) => warn!(key = key.name(), ?source, "ignoring an invalid value: {e}"),
				}
			}
		}
		(key.default_stored(), key.default_json(), Source::Default)
	}

	fn register(&mut self, key: &'static dyn Setting) {
		if let Some(slot) = self.slots.get(key.name()) {
			if slot.key.value_type() != key.value_type() {
				warn!(
					key = key.name(),
					"two keys of different types share a name; keeping the first"
				);
			}
			return;
		}
		let (stored, json, source) = Self::resolve(key, self.layers.get(key.name()));
		let (value, _) = watch::channel(stored);
		self.slots.insert(key.name(), Slot { key, source, json, value });
	}

	/// Recompute a registered key after its layers changed.
	fn update(&mut self, name: &str) -> Option<Source> {
		let slot = self.slots.get_mut(name)?;
		let (stored, json, source) = Self::resolve(slot.key, self.layers.get(name));
		slot.source = source;
		if slot.json != json {
			slot.json = json;
			slot.value.send_replace(stored);
		}
		Some(source)
	}
}

enum WriteOp {
	Put(String, String),
	Delete(String),
	/// Answered once everything before it is written.
	Flush(std_mpsc::Sender<Option<String>>),
}

struct Writer {
	tx: std_mpsc::Sender<WriteOp>,
	thread: JoinHandle<()>,
}

struct Inner {
	state: RwLock<State>,
	changes: broadcast::Sender<SettingChange>,
	writer: Option<Writer>,
}

impl Drop for Inner {
	fn drop(&mut self) {
		// Closing the channel ends the writer after the pending writes.
		if let Some(Writer { tx, thread }) = self.writer.take() {
			drop(tx);
			let _ = thread.join();
		}
	}
}

/// The settings service. Cheap to clone; all clones share the values.
#[derive(Clone)]
pub struct Settings {
	inner: Arc<Inner>,
}

impl fmt::Debug for Settings {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let state = self.read();
		f.debug_struct("Settings")
			.field("keys", &state.slots.len())
			.field("persistent", &self.inner.writer.is_some())
			.finish()
	}
}

impl Default for Settings {
	fn default() -> Self {
		Self::in_memory()
	}
}

/// Stored in the settings table: how many migrations ran.
const VERSION_KEY: &str = "settings.version";

impl Settings {
	/// Settings that are not stored (tests, tools, an engine without a database).
	pub fn in_memory() -> Self {
		Self::build(HashMap::new(), None)
	}

	/// Settings stored in the client database at `path` (its own connection).
	pub fn open(path: &Path) -> Result<Self, SettingsError> {
		Self::with_store(Store::open(path)?)
	}

	/// Settings stored in `store`, which moves to the writer thread.
	pub fn with_store(mut store: Store) -> Result<Self, SettingsError> {
		let mut version = 0;
		let mut values = HashMap::new();
		for (key, text) in store.settings_json()? {
			match serde_json::from_str::<Value>(&text) {
				Ok(v) if key == VERSION_KEY => version = v.as_u64().unwrap_or(0) as usize,
				Ok(v) => {
					values.insert(key, v);
				}
				Err(e) => warn!(key, "ignoring a stored setting that is not JSON: {e}"),
			}
		}
		if version < MIGRATIONS.len() {
			let mut migrator = Migrator { values, changes: BTreeMap::new() };
			for (i, migration) in MIGRATIONS.iter().enumerate().skip(version) {
				debug!(version = i + 1, "migrating settings");
				migration(&mut migrator);
			}
			let texts: Vec<(String, Option<String>)> = migrator
				.changes
				.iter()
				.map(|(k, v)| (k.clone(), v.as_ref().map(Value::to_string)))
				.chain([(VERSION_KEY.to_owned(), Some(MIGRATIONS.len().to_string()))])
				.collect();
			store.write_settings(texts.iter().map(|(k, v)| (k.as_str(), v.as_deref())))?;
			values = migrator.values;
		}
		let (tx, rx) = std_mpsc::channel();
		let thread = std::thread::Builder::new()
			.name("settings-writer".into())
			.spawn(move || write_loop(store, rx))
			.map_err(|e| SettingsError::Store(e.to_string()))?;
		Ok(Self::build(values, Some(Writer { tx, thread })))
	}

	fn build(runtime: HashMap<String, Value>, writer: Option<Writer>) -> Self {
		let layers = runtime
			.into_iter()
			.map(|(k, v)| (k, Layers { runtime: Some(v), ..Layers::default() }))
			.collect();
		let (changes, _) = broadcast::channel(1024);
		let settings = Self {
			inner: Arc::new(Inner {
				state: RwLock::new(State { layers, slots: HashMap::new() }),
				changes,
				writer,
			}),
		};
		for key in builtin_keys() {
			settings.register(key);
		}
		settings
	}

	fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
		self.inner.state.read().unwrap_or_else(PoisonError::into_inner)
	}

	fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
		self.inner.state.write().unwrap_or_else(PoisonError::into_inner)
	}

	/// Make a key known (keys register themselves on first use, too).
	pub fn register(&self, key: &'static dyn Setting) {
		if !self.read().slots.contains_key(key.name()) {
			self.write().register(key);
		}
	}

	/// The registered keys, by name.
	pub fn keys(&self) -> Vec<&'static dyn Setting> {
		let mut keys: Vec<_> = self.read().slots.values().map(|s| s.key).collect();
		keys.sort_by_key(|k| k.name());
		keys
	}

	/// The current value.
	pub fn get<T>(&self, key: &'static Key<T>) -> T
	where
		T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
	{
		T::clone(&self.get_arc(key))
	}

	/// The current value, without copying it.
	pub fn get_arc<T>(&self, key: &'static Key<T>) -> Arc<T>
	where
		T: Serialize + DeserializeOwned + Send + Sync + 'static,
	{
		let stored = {
			let state = self.read();
			state.slots.get(key.name).map(|s| s.value.borrow().clone())
		};
		let stored = stored.unwrap_or_else(|| {
			self.register(key);
			self.read().slots[key.name].value.borrow().clone()
		});
		stored.downcast().unwrap_or_else(|_| Arc::new(key.default_value()))
	}

	/// The current value of a key by name, as JSON (`None`: not registered).
	pub fn get_json(&self, name: &str) -> Option<Value> {
		self.read().slots.get(name).map(|s| s.json.clone())
	}

	/// Where the value of a key comes from (`None`: not registered).
	pub fn source(&self, name: &str) -> Option<Source> {
		self.read().slots.get(name).map(|s| s.source)
	}

	/// Set the runtime value (stored, and ahead of overrides and config).
	pub fn set<T>(&self, key: &'static Key<T>, value: T) -> Result<(), SettingsError>
	where
		T: Serialize + DeserializeOwned + Send + Sync + 'static,
	{
		let invalid = |message: String| SettingsError::Invalid { key: key.name.into(), message };
		key.validate(&value).map_err(invalid)?;
		let json = serde_json::to_value(&value).map_err(|e| invalid(e.to_string()))?;
		self.register(key);
		self.store_runtime(key.name, json, Arc::new(value));
		Ok(())
	}

	/// Set the runtime value of a registered key from JSON.
	pub fn set_json(&self, name: &str, value: Value) -> Result<(), SettingsError> {
		let key = self.key(name)?;
		let (stored, json) = key
			.parse_json(&value)
			.map_err(|message| SettingsError::Invalid { key: name.into(), message })?;
		self.store_runtime(key.name(), json, stored);
		Ok(())
	}

	fn key(&self, name: &str) -> Result<&'static dyn Setting, SettingsError> {
		self.read()
			.slots
			.get(name)
			.map(|s| s.key)
			.ok_or_else(|| SettingsError::UnknownKey(name.into()))
	}

	fn store_runtime(&self, name: &'static str, json: Value, stored: Stored) {
		let text = json.to_string();
		{
			let mut state = self.write();
			state.layers.entry(name.to_owned()).or_default().runtime = Some(json.clone());
			let slot = state.slots.get_mut(name).expect("registered");
			slot.source = Source::Runtime;
			if slot.json != json {
				slot.json = json;
				slot.value.send_replace(stored);
			}
		}
		self.persist(WriteOp::Put(name.to_owned(), text));
		self.announce(name, Some(Source::Runtime));
	}

	/// Remove the runtime value: the override, config or default applies again.
	pub fn reset(&self, name: &str) -> Result<(), SettingsError> {
		let source = {
			let mut state = self.write();
			if let Some(layers) = state.layers.get_mut(name) {
				layers.runtime = None;
			}
			state.update(name)
		};
		self.persist(WriteOp::Delete(name.to_owned()));
		self.announce(name, source);
		Ok(())
	}

	/// Set the value of a layer below runtime; checked now if the key is
	/// registered, else when it registers.
	fn set_layer(
		&self,
		name: &str,
		value: Value,
		layer: fn(&mut Layers) -> &mut Option<Value>,
	) -> Result<(), SettingsError> {
		let value = match self.key(name) {
			Ok(key) => {
				key.parse_json(&value)
					.map_err(|message| SettingsError::Invalid { key: name.into(), message })?
					.1
			}
			Err(_) => value,
		};
		let mut state = self.write();
		*layer(state.layers.entry(name.to_owned()).or_default()) = Some(value);
		let effective = |state: &State| state.slots.get(name).map(|s| (s.json.clone(), s.source));
		let before = effective(&state);
		let source = state.update(name);
		let changed = before != effective(&state);
		drop(state);
		// Announced only if it changed what applies.
		if changed {
			self.announce(name, source);
		}
		Ok(())
	}

	/// A command line or environment value (below runtime values).
	pub fn set_override(&self, name: &str, value: Value) -> Result<(), SettingsError> {
		self.set_layer(name, value, |l| &mut l.overrides)
	}

	/// A config file value (below overrides).
	pub fn set_config(&self, name: &str, value: Value) -> Result<(), SettingsError> {
		self.set_layer(name, value, |l| &mut l.config)
	}

	/// Load a config file: TOML, or JSON if the name ends in `.json`. Tables
	/// nest key names (`[stream]` `fps = 90` is `stream.fps`) unless the
	/// table's name is a registered key (e.g. `[audio]`). Returns the
	/// invalid entries, which are skipped.
	pub fn load_config_file(&self, path: &Path) -> Result<Vec<SettingsError>, SettingsError> {
		let config_error =
			|message: String| SettingsError::Config { path: path.display().to_string(), message };
		let text = std::fs::read_to_string(path).map_err(|e| config_error(e.to_string()))?;
		let root: Value = if path.extension().is_some_and(|e| e == "json") {
			serde_json::from_str(&text).map_err(|e| config_error(e.to_string()))?
		} else {
			let table: toml::Table =
				toml::from_str(&text).map_err(|e| config_error(e.to_string()))?;
			serde_json::to_value(table).map_err(|e| config_error(e.to_string()))?
		};
		let Value::Object(root) = root else {
			return Err(config_error("expected a table of settings".into()));
		};
		let mut entries = Vec::new();
		self.flatten(String::new(), root, &mut entries);
		Ok(entries
			.into_iter()
			.filter_map(|(name, value)| self.set_config(&name, value).err())
			.collect())
	}

	fn flatten(
		&self,
		prefix: String,
		table: serde_json::Map<String, Value>,
		out: &mut Vec<(String, Value)>,
	) {
		for (k, v) in table {
			let name = if prefix.is_empty() { k } else { format!("{prefix}.{k}") };
			match v {
				Value::Object(inner) if !self.read().slots.contains_key(name.as_str()) => {
					self.flatten(name, inner, out);
				}
				v => out.push((name, v)),
			}
		}
	}

	/// Overrides from the environment: `VOELIN_SETTING_STREAM__FPS=90` sets
	/// `stream.fps` (`__` separates name parts; the rest is lower-cased).
	/// Values are JSON, or else strings. Returns the invalid ones.
	pub fn apply_env(
		&self,
		vars: impl IntoIterator<Item = (String, String)>,
	) -> Vec<SettingsError> {
		const PREFIX: &str = "VOELIN_SETTING_";
		vars.into_iter()
			.filter_map(|(var, value)| {
				let name = var.strip_prefix(PREFIX)?.to_lowercase().replace("__", ".");
				self.set_override(&name, parse_text(&value)).err()
			})
			.collect()
	}

	/// Overrides from `key=value` pairs (e.g. `--set stream.fps=90`); values
	/// are JSON, or else strings. Returns the invalid ones.
	pub fn apply_overrides<'a>(
		&self,
		pairs: impl IntoIterator<Item = &'a str>,
	) -> Vec<SettingsError> {
		pairs
			.into_iter()
			.filter_map(|pair| match pair.split_once('=') {
				Some((name, value)) => self.set_override(name.trim(), parse_text(value)).err(),
				None => Some(SettingsError::Invalid {
					key: pair.into(),
					message: "expected key=value".into(),
				}),
			})
			.collect()
	}

	/// Every set and reset, of any key.
	pub fn subscribe(&self) -> broadcast::Receiver<SettingChange> {
		self.inner.changes.subscribe()
	}

	/// The value of one key, updated when it changes.
	pub fn watch<T>(&self, key: &'static Key<T>) -> SettingWatch<T>
	where
		T: Serialize + DeserializeOwned + Send + Sync + 'static,
	{
		self.register(key);
		let rx = self.read().slots[key.name].value.subscribe();
		SettingWatch { key, rx, _type: PhantomData }
	}

	/// Wait until the writes so far are in the database.
	pub fn flush(&self) -> Result<(), SettingsError> {
		let Some(writer) = &self.inner.writer else { return Ok(()) };
		let (tx, rx) = std_mpsc::channel();
		if writer.tx.send(WriteOp::Flush(tx)).is_err() {
			return Err(SettingsError::Store("the writer stopped".into()));
		}
		match rx.recv() {
			Ok(None) => Ok(()),
			Ok(Some(e)) => Err(SettingsError::Store(e)),
			Err(_) => Err(SettingsError::Store("the writer stopped".into())),
		}
	}

	fn persist(&self, op: WriteOp) {
		if let Some(writer) = &self.inner.writer {
			let _ = writer.tx.send(op);
		}
	}

	fn announce(&self, name: &str, source: Option<Source>) {
		let _ = self.inner.changes.send(SettingChange { key: name.to_owned(), source });
	}
}

/// JSON if it parses, else the text as a string (`vp9` → `"vp9"`).
fn parse_text(text: &str) -> Value {
	serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// Writes batches of changes, the last change of a key winning.
fn write_loop(mut store: Store, rx: std_mpsc::Receiver<WriteOp>) {
	let mut error = None;
	while let Ok(first) = rx.recv() {
		let mut batch: BTreeMap<String, Option<String>> = BTreeMap::new();
		let mut flushes = Vec::new();
		for op in std::iter::once(first).chain(rx.try_iter()) {
			match op {
				WriteOp::Put(key, value) => {
					batch.insert(key, Some(value));
				}
				WriteOp::Delete(key) => {
					batch.insert(key, None);
				}
				WriteOp::Flush(done) => flushes.push(done),
			}
		}
		if !batch.is_empty()
			&& let Err(e) =
				store.write_settings(batch.iter().map(|(k, v)| (k.as_str(), v.as_deref())))
		{
			warn!("cannot store settings: {e}");
			error = Some(e.to_string());
		}
		for done in flushes {
			let _ = done.send(error.take());
		}
	}
}

/// The value of one key; see [`Settings::watch`].
pub struct SettingWatch<T: 'static> {
	key: &'static Key<T>,
	rx: watch::Receiver<Stored>,
	_type: PhantomData<fn() -> T>,
}

impl<T: Send + Sync + 'static> SettingWatch<T> {
	/// The current value.
	pub fn get(&self) -> Arc<T> {
		let stored = self.rx.borrow().clone();
		stored.downcast().unwrap_or_else(|_| Arc::new(self.key.default_value()))
	}

	/// Whether the value changed since it was last read with [`Self::changed`]
	/// or [`Self::get_new`].
	pub fn has_changed(&self) -> bool {
		self.rx.has_changed().unwrap_or(false)
	}

	/// The value, if it changed since the last call (for polling threads).
	pub fn get_new(&mut self) -> Option<Arc<T>> {
		if !self.has_changed() {
			return None;
		}
		self.rx.mark_unchanged();
		Some(self.get())
	}

	/// Wait for the next change; `None` once the settings are gone.
	pub async fn changed(&mut self) -> Option<Arc<T>> {
		self.rx.changed().await.ok()?;
		Some(self.get())
	}
}

/// Settings stored before this code are upgraded by these, in order; the
/// database remembers how many ran (`settings.version`). Add new ones at the
/// end; never change one that shipped.
const MIGRATIONS: &[fn(&mut Migrator)] = &[split_ui_blob];

/// The runtime values during a migration.
struct Migrator {
	values: HashMap<String, Value>,
	/// Written after the migrations (`None`: removed).
	changes: BTreeMap<String, Option<Value>>,
}

impl Migrator {
	fn get(&self, key: &str) -> Option<&Value> {
		self.values.get(key)
	}

	fn set_if_absent(&mut self, key: &str, value: Value) {
		if !self.values.contains_key(key) {
			self.values.insert(key.to_owned(), value.clone());
			self.changes.insert(key.to_owned(), Some(value));
		}
	}
}

/// The share dialog's presets before the stream keys existed, by index.
const LEGACY_FPS: [u32; 3] = [15, 30, 60];
const LEGACY_BITRATES: [u32; 4] = [2500, 4608, 8000, 10_000];

/// Version 1: the UI kept everything in its `ui` blob. Copy what now has
/// its own key: the share dialog's last frame rate and bitrate (unless they
/// were the old defaults, 30 fps and 4608 kbit/s, which the user most likely
/// never chose) and the crash report opt-in (which the Android app read
/// from `crash_reports` already). The blob stays for older versions.
fn split_ui_blob(m: &mut Migrator) {
	let Some(ui) = m.get("ui").cloned() else { return };
	let share = ui.get("share");
	let index = |field: &str| share.and_then(|s| s.get(field)).and_then(Value::as_u64);
	if let Some(fps) = index("fps_index").and_then(|i| LEGACY_FPS.get(i as usize))
		&& *fps != 30
	{
		m.set_if_absent(STREAM_FPS.name, json!(fps));
	}
	if let Some(kbps) = index("bitrate_index").and_then(|i| LEGACY_BITRATES.get(i as usize))
		&& *kbps != 4608
	{
		m.set_if_absent(STREAM_BITRATE_KBPS.name, json!(kbps));
	}
	if let Some(on) = ui.get("crash_reports").and_then(Value::as_bool) {
		m.set_if_absent(CRASH_REPORTS.name, json!(on));
	}
}

// The built-in keys.

/// Name of [`CRASH_REPORTS`], for code that reads the database directly.
pub const CRASH_REPORTS_KEY: &str = "crash_reports";

/// Write crash reports to disk (opt-in; never uploaded).
pub static CRASH_REPORTS: Key<bool> = Key::new(
	CRASH_REPORTS_KEY,
	Kind::Bool,
	"Write crash reports to disk (never uploaded).",
	bool::default,
);

/// All audio settings of the sessions, in one value (the `audio` blob).
pub static AUDIO: Key<AudioSettings> = Key::new(
	"audio",
	Kind::Json,
	"Audio devices, processing and transmit mode.",
	AudioSettings::default,
);

fn at_least_one(v: &u32) -> Result<(), String> {
	if *v == 0 { Err("must be at least 1".into()) } else { Ok(()) }
}

/// Frame rate of our stream (no maximum).
pub static STREAM_FPS: Key<u32> =
	Key::new("stream.fps", Kind::UInt { min: 1 }, "Frames per second of our stream.", || 60)
		.validated(at_least_one);

/// Video bitrate of our stream in kbit/s (no maximum).
pub static STREAM_BITRATE_KBPS: Key<u32> = Key::new(
	"stream.bitrate_kbps",
	Kind::UInt { min: 1 },
	"Video bitrate of our stream in kbit/s.",
	|| 8000,
)
.validated(at_least_one);

/// One simulcast layer as stored; see [`LayerSpec`] for the fields. The
/// serde form of a layer: `{"scale": 0.5, "max_fps": 30, "bitrate": 1500000}`
/// or `{"size": [1280, 720], "bitrate": 4000000, "rid": "h"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerSetting {
	/// Default: the position in the list.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub id: Option<LayerId>,
	pub scale: f32,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub size: Option<(u32, u32)>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_fps: Option<u32>,
	/// Bit/s.
	pub bitrate: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_bitrate: Option<u64>,
	pub min_bitrate: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub rid: Option<String>,
}

impl Default for LayerSetting {
	fn default() -> Self {
		let spec = LayerSpec::single(0);
		Self {
			id: None,
			scale: spec.scale,
			size: spec.size,
			max_fps: spec.max_fps,
			bitrate: spec.bitrate,
			max_bitrate: spec.max_bitrate,
			min_bitrate: spec.min_bitrate,
			rid: spec.rid,
		}
	}
}

impl LayerSetting {
	/// The layer as [`LayerSpec`]; `index` is its position in the list.
	pub fn to_spec(&self, index: usize) -> LayerSpec {
		LayerSpec {
			id: self.id.unwrap_or(index as LayerId),
			scale: self.scale,
			size: self.size,
			max_fps: self.max_fps,
			bitrate: self.bitrate,
			max_bitrate: self.max_bitrate,
			min_bitrate: self.min_bitrate,
			rid: self.rid.clone(),
		}
	}

	pub fn from_spec(spec: &LayerSpec) -> Self {
		Self {
			id: Some(spec.id),
			scale: spec.scale,
			size: spec.size,
			max_fps: spec.max_fps,
			bitrate: spec.bitrate,
			max_bitrate: spec.max_bitrate,
			min_bitrate: spec.min_bitrate,
			rid: spec.rid.clone(),
		}
	}
}

/// The layers of [`STREAM_LAYERS`] as [`LayerSpec`]s (empty: a single layer).
pub fn layer_specs(layers: &[LayerSetting]) -> Vec<LayerSpec> {
	layers.iter().enumerate().map(|(i, l)| l.to_spec(i)).collect()
}

#[allow(clippy::ptr_arg)] // a validation of `Key<Vec<_>>` is `fn(&Vec<_>)`
fn valid_layers(layers: &Vec<LayerSetting>) -> Result<(), String> {
	let specs = layer_specs(layers);
	for (i, s) in specs.iter().enumerate() {
		if s.bitrate == 0 {
			return Err(format!("layer {}: bitrate must be above 0", s.id));
		}
		if s.size.is_none() && !(s.scale.is_finite() && s.scale > 0.0) {
			return Err(format!("layer {}: scale must be above 0", s.id));
		}
		if s.size.is_some_and(|(w, h)| w == 0 || h == 0) {
			return Err(format!("layer {}: size must not be 0", s.id));
		}
		if s.max_fps == Some(0) {
			return Err(format!("layer {}: max_fps must be at least 1", s.id));
		}
		if s.max_bitrate.is_some_and(|max| max < s.bitrate) {
			return Err(format!("layer {}: max_bitrate is below bitrate", s.id));
		}
		if specs[..i].iter().any(|o| o.id == s.id) {
			return Err(format!("layer id {} appears twice", s.id));
		}
	}
	Ok(())
}

/// Simulcast layers of our stream; empty: one layer.
pub static STREAM_LAYERS: Key<Vec<LayerSetting>> = Key::new(
	"stream.layers",
	Kind::Json,
	"Simulcast layers of our stream (empty: one layer).",
	Vec::new,
)
.validated(valid_layers);

/// Video codec of our stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecChoice {
	/// The best one both ends support.
	#[default]
	Auto,
	Vp8,
	Vp9,
	H264,
	Av1,
}

pub static STREAM_CODEC: Key<CodecChoice> = Key::new(
	"stream.codec",
	Kind::Choice(&["auto", "vp8", "vp9", "h264", "av1"]),
	"Video codec of our stream.",
	CodecChoice::default,
);

/// How the screen is captured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureChoice {
	#[default]
	Auto,
	/// The desktop's ScreenCast portal (Wayland, Flatpak).
	Portal,
	/// wlroots screencopy.
	Wlroots,
	X11,
}

pub static STREAM_CAPTURE_BACKEND: Key<CaptureChoice> = Key::new(
	"stream.capture_backend",
	Kind::Choice(&["auto", "portal", "wlroots", "x11"]),
	"How the screen is captured.",
	CaptureChoice::default,
);

#[allow(clippy::ptr_arg)] // a validation of `Key<String>` is `fn(&String)`
fn not_empty(s: &String) -> Result<(), String> {
	if s.trim().is_empty() { Err("must not be empty".into()) } else { Ok(()) }
}

/// Video encoder: `auto`, `software`, or the name of an encoder backend.
pub static STREAM_ENCODER_BACKEND: Key<String> = Key::new(
	"stream.encoder_backend",
	Kind::Text { suggestions: &["auto", "software"] },
	"Video encoder: auto, software, or an encoder's name.",
	|| "auto".into(),
)
.validated(not_empty);

/// SRTP protection profiles the WebRTC stack knows.
pub const SRTP_PROFILES: &[&str] = &["AES128_CM_SHA1_80", "AEAD_AES_128_GCM", "AEAD_AES_256_GCM"];

#[allow(clippy::ptr_arg)] // a validation of `Key<Vec<_>>` is `fn(&Vec<_>)`
fn valid_srtp(list: &Vec<String>) -> Result<(), String> {
	if list.is_empty() {
		return Err("at least one profile is needed".into());
	}
	for (i, p) in list.iter().enumerate() {
		if !SRTP_PROFILES.contains(&p.as_str()) {
			return Err(format!("unknown profile {p} (known: {})", SRTP_PROFILES.join(", ")));
		}
		if list[..i].contains(p) {
			return Err(format!("{p} appears twice"));
		}
	}
	Ok(())
}

/// SRTP profiles offered for stream media, most preferred first.
pub static STREAM_SRTP_PROFILES: Key<Vec<String>> = Key::new(
	"stream.srtp_profiles",
	Kind::List(SRTP_PROFILES),
	"SRTP profiles for stream media, most preferred first.",
	|| SRTP_PROFILES.iter().map(|p| (*p).to_owned()).collect(),
)
.validated(valid_srtp);

/// Use hardware video encoders and decoders where available.
pub static STREAM_HARDWARE_ACCELERATION: Key<bool> = Key::new(
	"stream.hardware_acceleration",
	Kind::Bool,
	"Use hardware video encoders and decoders where available.",
	|| true,
);

/// Who may watch our stream without being asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamPermissions {
	/// Every join request is accepted.
	Everyone,
	/// Friends are accepted. Until there are contacts, every request is
	/// shown to the user ([`crate::Event::StreamViewerRequest`]).
	Friends,
	/// Clients in our channel are accepted, others denied.
	#[default]
	Channel,
	/// Every join request is denied.
	Nobody,
}

pub static STREAM_PERMISSIONS: Key<StreamPermissions> = Key::new(
	"stream.permissions",
	Kind::Choice(&["everyone", "friends", "channel", "nobody"]),
	"Who may watch our stream without being asked.",
	StreamPermissions::default,
);

/// The keys every [`Settings`] knows from the start.
pub fn builtin_keys() -> [&'static dyn Setting; 11] {
	[
		&CRASH_REPORTS,
		&AUDIO,
		&STREAM_FPS,
		&STREAM_BITRATE_KBPS,
		&STREAM_LAYERS,
		&STREAM_CODEC,
		&STREAM_CAPTURE_BACKEND,
		&STREAM_ENCODER_BACKEND,
		&STREAM_SRTP_PROFILES,
		&STREAM_HARDWARE_ACCELERATION,
		&STREAM_PERMISSIONS,
	]
}

/// The engine's settings: one [`Settings`] that [`crate::Command::AttachSettings`]
/// can replace, shared by the engine and its sessions.
#[derive(Clone, Debug, Default)]
pub(crate) struct SharedSettings(Arc<RwLock<Settings>>);

impl SharedSettings {
	pub fn new(settings: Settings) -> Self {
		Self(Arc::new(RwLock::new(settings)))
	}

	pub fn current(&self) -> Settings {
		self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
	}

	pub fn replace(&self, settings: Settings) {
		*self.0.write().unwrap_or_else(PoisonError::into_inner) = settings;
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	fn temp_db(tag: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"voelin-settings-{tag}-{}-{:?}",
			std::process::id(),
			std::thread::current().id()
		));
		let _ = std::fs::remove_dir_all(&dir);
		dir.join("client.db")
	}

	#[test]
	fn defaults_and_registry() {
		let s = Settings::in_memory();
		assert_eq!(s.get(&STREAM_FPS), 60);
		assert_eq!(s.get(&STREAM_BITRATE_KBPS), 8000);
		assert!(s.get(&STREAM_LAYERS).is_empty());
		assert_eq!(s.get(&STREAM_CODEC), CodecChoice::Auto);
		assert_eq!(s.get(&STREAM_PERMISSIONS), StreamPermissions::Channel);
		assert_eq!(
			s.get(&STREAM_SRTP_PROFILES),
			["AES128_CM_SHA1_80", "AEAD_AES_128_GCM", "AEAD_AES_256_GCM"]
		);
		assert!(s.get(&STREAM_HARDWARE_ACCELERATION));
		assert_eq!(s.get(&STREAM_ENCODER_BACKEND), "auto");
		assert_eq!(s.source("stream.fps"), Some(Source::Default));
		let names: Vec<_> = s.keys().iter().map(|k| k.name()).collect();
		assert!(names.contains(&"stream.permissions") && names.contains(&"audio"));
		assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted: {names:?}");
		assert_eq!(s.get_json("stream.codec"), Some(json!("auto")));
		assert_eq!(s.get_json("nope"), None);
	}

	#[test]
	fn precedence_and_reset() {
		let s = Settings::in_memory();
		s.set_config("stream.fps", json!(24)).unwrap();
		assert_eq!((s.get(&STREAM_FPS), s.source("stream.fps")), (24, Some(Source::Config)));
		assert!(s.apply_overrides(["stream.fps=90"]).is_empty());
		assert_eq!((s.get(&STREAM_FPS), s.source("stream.fps")), (90, Some(Source::Override)));
		s.set(&STREAM_FPS, 144).unwrap();
		assert_eq!((s.get(&STREAM_FPS), s.source("stream.fps")), (144, Some(Source::Runtime)));
		// A later config or override does not beat the runtime value.
		s.set_config("stream.fps", json!(10)).unwrap();
		s.set_override("stream.fps", json!(20)).unwrap();
		assert_eq!(s.get(&STREAM_FPS), 144);
		s.reset("stream.fps").unwrap();
		assert_eq!((s.get(&STREAM_FPS), s.source("stream.fps")), (20, Some(Source::Override)));

		// Environment names; strings without quotes.
		let env = [
			("VOELIN_SETTING_STREAM__CODEC".to_owned(), "vp9".to_owned()),
			("VOELIN_SETTING_STREAM__BITRATE_KBPS".to_owned(), "25000".to_owned()),
			("OTHER".to_owned(), "x".to_owned()),
		];
		assert!(s.apply_env(env).is_empty());
		assert_eq!(s.get(&STREAM_CODEC), CodecChoice::Vp9);
		assert_eq!(s.get(&STREAM_BITRATE_KBPS), 25_000);
	}

	#[test]
	fn validation() {
		let s = Settings::in_memory();
		let invalid =
			|r: Result<(), SettingsError>| matches!(r, Err(SettingsError::Invalid { .. }));
		assert!(invalid(s.set(&STREAM_FPS, 0)));
		assert!(invalid(s.set_json("stream.fps", json!("fast"))));
		assert!(invalid(s.set_json("stream.codec", json!("mpeg2"))));
		assert!(invalid(s.set_json("stream.srtp_profiles", json!([]))));
		assert!(invalid(
			s.set_json("stream.srtp_profiles", json!(["AEAD_AES_128_GCM", "AEAD_AES_128_GCM"]))
		));
		assert!(invalid(s.set_json("stream.layers", json!([{"bitrate": 0}]))));
		assert!(invalid(
			s.set_json("stream.layers", json!([{"id": 1, "bitrate": 5}, {"id": 1, "bitrate": 5}]))
		));
		assert!(invalid(s.set_override("stream.permissions", json!("strangers"))));
		assert!(matches!(s.set_json("stream.nope", json!(1)), Err(SettingsError::UnknownKey(_))));
		assert_eq!(s.apply_overrides(["stream.fps"]).len(), 1, "no =");
		// Nothing changed.
		assert_eq!(s.get(&STREAM_FPS), 60);
		// No maximum.
		s.set_json("stream.fps", json!(1000)).unwrap();
		s.set(&STREAM_BITRATE_KBPS, u32::MAX).unwrap();
		assert_eq!(s.get(&STREAM_BITRATE_KBPS), u32::MAX);
	}

	#[test]
	fn layers_convert_to_specs() {
		let s = Settings::in_memory();
		s.set_json(
			"stream.layers",
			json!([{"scale": 0.5, "max_fps": 30, "bitrate": 1_500_000, "rid": "l"},
				{"size": [1920, 1080], "bitrate": 6_000_000, "max_bitrate": 9_000_000}]),
		)
		.unwrap();
		let specs = layer_specs(&s.get(&STREAM_LAYERS));
		assert_eq!(specs.len(), 2);
		assert_eq!((specs[0].id, specs[0].scale, specs[0].rid.as_deref()), (0, 0.5, Some("l")));
		assert_eq!(
			(specs[1].id, specs[1].size, specs[1].max_bitrate),
			(1, Some((1920, 1080)), Some(9_000_000))
		);
		assert_eq!(LayerSetting::from_spec(&specs[1]).to_spec(7), specs[1]);
	}

	#[tokio::test]
	async fn notifications() {
		let s = Settings::in_memory();
		let mut all = s.subscribe();
		let mut fps = s.watch(&STREAM_FPS);
		assert_eq!(*fps.get(), 60);
		assert!(!fps.has_changed());
		s.set(&STREAM_FPS, 90).unwrap();
		assert_eq!(
			*tokio::time::timeout(Duration::from_secs(1), fps.changed()).await.unwrap().unwrap(),
			90
		);
		assert_eq!(
			all.recv().await.unwrap(),
			SettingChange { key: "stream.fps".into(), source: Some(Source::Runtime) }
		);
		// A set of the same value is announced, but the value did not change.
		s.set(&STREAM_FPS, 90).unwrap();
		assert_eq!(all.recv().await.unwrap().key, "stream.fps");
		assert!(fps.get_new().is_none());
		s.reset("stream.fps").unwrap();
		assert_eq!(
			all.recv().await.unwrap(),
			SettingChange { key: "stream.fps".into(), source: Some(Source::Default) }
		);
		assert_eq!(fps.get_new().as_deref(), Some(&60));
		// Other keys do not wake this watch.
		s.set(&STREAM_CODEC, CodecChoice::Av1).unwrap();
		assert!(!fps.has_changed());
		assert_eq!(all.recv().await.unwrap().key, "stream.codec");
	}

	#[test]
	fn persistence_across_reopen() {
		let path = temp_db("reopen");
		{
			let s = Settings::open(&path).unwrap();
			s.set(&STREAM_FPS, 75).unwrap();
			s.set(&STREAM_PERMISSIONS, StreamPermissions::Nobody).unwrap();
			s.set(&STREAM_CODEC, CodecChoice::H264).unwrap();
			s.reset("stream.codec").unwrap();
			s.set_override("stream.bitrate_kbps", json!(123)).unwrap();
			s.flush().unwrap();
		}
		let s = Settings::open(&path).unwrap();
		assert_eq!(s.get(&STREAM_FPS), 75);
		assert_eq!(s.get(&STREAM_PERMISSIONS), StreamPermissions::Nobody);
		assert_eq!(s.source("stream.codec"), Some(Source::Default));
		assert_eq!(
			s.source("stream.bitrate_kbps"),
			Some(Source::Default),
			"overrides are not stored"
		);
		drop(s);
		// Written without flush: the writer finishes when the settings are dropped.
		{
			let s = Settings::open(&path).unwrap();
			s.set(&STREAM_FPS, 76).unwrap();
		}
		assert_eq!(Settings::open(&path).unwrap().get(&STREAM_FPS), 76);
		std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	/// A key another crate defines (like the UI's blobs).
	#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
	#[serde(default)]
	struct Blob {
		a: u32,
		b: String,
	}

	static BLOB: Key<Blob> = Key::new("blob", Kind::Json, "A test blob.", Blob::default);
	/// The UI's blob, as the UI registers it.
	static UI: Key<Value> = Key::new("ui", Kind::Json, "The UI's settings.", || Value::Null);
	static LATER: Key<u32> = Key::new("later.number", Kind::UInt { min: 0 }, "A test key.", || 1);

	#[test]
	fn old_blobs_migrate_and_stay() {
		let path = temp_db("migrate");
		{
			let store = Store::open(&path).unwrap();
			let ui = json!({"global_ptt": true, "ptt_key": "F9", "crash_reports": true,
				"share": {"fps_index": 2, "bitrate_index": 1, "audio": false, "auto_accept": true}});
			store.set_setting("ui", &ui).unwrap();
			store
				.set_setting("audio", &json!({"transmit": "continuous", "output_volume": 0.5}))
				.unwrap();
			store.set_setting("blob", &json!({"a": 3})).unwrap();
		}
		let s = Settings::open(&path).unwrap();
		// The chosen 60 fps is copied; the old default bitrate is not.
		assert_eq!((s.get(&STREAM_FPS), s.source("stream.fps")), (60, Some(Source::Runtime)));
		assert_eq!(s.source("stream.bitrate_kbps"), Some(Source::Default));
		assert!(s.get(&CRASH_REPORTS));
		// The old blobs are keys as before; partial ones fill in defaults.
		let audio = s.get(&AUDIO);
		assert_eq!((audio.transmit, audio.output_volume), (crate::TransmitMode::Continuous, 0.5));
		assert_eq!(s.get(&BLOB), Blob { a: 3, b: String::new() });
		assert_eq!(s.get_json("ui"), None, "not a core key");
		assert_eq!(s.get(&UI)["ptt_key"], "F9", "registered later");
		// Runs once: a later change is not overwritten by the blob.
		s.set(&STREAM_FPS, 50).unwrap();
		drop(s);
		let s = Settings::open(&path).unwrap();
		assert_eq!(s.get(&STREAM_FPS), 50);
		drop(s);
		let store = Store::open(&path).unwrap();
		assert!(store.setting::<Value>("ui").unwrap().is_some(), "the blob stays");
		assert_eq!(store.setting::<usize>(VERSION_KEY).unwrap(), Some(MIGRATIONS.len()));
		drop(store);
		std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn config_files() {
		let path = temp_db("config");
		let dir = path.parent().unwrap();
		std::fs::create_dir_all(dir).unwrap();
		let toml = dir.join("config.toml");
		std::fs::write(
			&toml,
			"\"stream.codec\" = \"vp8\"\n[stream]\nfps = 30\npermissions = \"loud\"\n\
			 [audio]\ntransmit = \"voice_activation\"\n",
		)
		.unwrap();
		let s = Settings::in_memory();
		let errors = s.load_config_file(&toml).unwrap();
		assert_eq!(errors.len(), 1, "{errors:?}");
		assert_eq!(s.get(&STREAM_CODEC), CodecChoice::Vp8);
		assert_eq!((s.get(&STREAM_FPS), s.source("stream.fps")), (30, Some(Source::Config)));
		assert_eq!(s.get(&AUDIO).transmit, crate::TransmitMode::VoiceActivation);
		// Object values need their key registered first; other keys wait
		// for their registration.
		let json = dir.join("config.json");
		std::fs::write(&json, r#"{"blob": {"b": "x"}, "later": {"number": 5}}"#).unwrap();
		s.register(&BLOB);
		assert!(s.load_config_file(&json).unwrap().is_empty());
		assert_eq!(s.get(&BLOB).b, "x");
		assert_eq!((s.get(&LATER), s.source("later.number")), (5, Some(Source::Config)));
		assert!(s.load_config_file(&dir.join("missing.toml")).is_err());
		std::fs::remove_dir_all(dir).unwrap();
	}
}
