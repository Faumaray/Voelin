//! ServerQuery text format: escaping, commands, responses and events.
//!
//! A response is zero or more data lines followed by `error id=0 msg=ok`.
//! Data is a list of rows separated by `|`, each row a space-separated list of
//! `key=value` pairs. Events are lines starting with `notify`.

use std::fmt;

/// Escape a value for the query protocol.
pub fn escape(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		match c {
			'\\' => out.push_str("\\\\"),
			'/' => out.push_str("\\/"),
			' ' => out.push_str("\\s"),
			'|' => out.push_str("\\p"),
			'\x07' => out.push_str("\\a"),
			'\x08' => out.push_str("\\b"),
			'\x0c' => out.push_str("\\f"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'\x0b' => out.push_str("\\v"),
			c => out.push(c),
		}
	}
	out
}

/// Reverse [`escape`]. Unknown escapes are kept as they are.
pub fn unescape(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut chars = s.chars();
	while let Some(c) = chars.next() {
		if c != '\\' {
			out.push(c);
			continue;
		}
		match chars.next() {
			Some('\\') => out.push('\\'),
			Some('/') => out.push('/'),
			Some('s') => out.push(' '),
			Some('p') => out.push('|'),
			Some('a') => out.push('\x07'),
			Some('b') => out.push('\x08'),
			Some('f') => out.push('\x0c'),
			Some('n') => out.push('\n'),
			Some('r') => out.push('\r'),
			Some('t') => out.push('\t'),
			Some('v') => out.push('\x0b'),
			Some(other) => {
				out.push('\\');
				out.push(other);
			}
			None => out.push('\\'),
		}
	}
	out
}

/// One `key=value ...` group of a response or event. Keys keep their order;
/// a key without `=` has an empty value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row(pub Vec<(String, String)>);

impl Row {
	pub fn get(&self, key: &str) -> Option<&str> {
		self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
	}

	pub fn parse<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
		self.get(key)?.parse().ok()
	}

	/// `1` / `0` flags.
	pub fn flag(&self, key: &str) -> Option<bool> {
		self.get(key).map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
	}

	pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
		let key = key.into();
		let value = value.into();
		match self.0.iter_mut().find(|(k, _)| *k == key) {
			Some(entry) => entry.1 = value,
			None => self.0.push((key, value)),
		}
	}
}

/// Parse `a=1 b=2|a=3` into rows. Keys shared by the first row are not
/// repeated by the server in later rows for some commands; callers that need
/// that should use [`Row::get`] on the first row as a fallback.
pub fn parse_rows(line: &str) -> Vec<Row> {
	line.split('|')
		.map(|part| {
			Row(part
				.split(' ')
				.filter(|kv| !kv.is_empty())
				.map(|kv| match kv.split_once('=') {
					Some((k, v)) => (k.to_string(), unescape(v)),
					None => (kv.to_string(), String::new()),
				})
				.collect())
		})
		.filter(|row| !row.0.is_empty())
		.collect()
}

/// The `error` line that ends every response.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct QueryError {
	pub id: u32,
	pub msg: String,
	pub extra_msg: Option<String>,
	pub failed_permid: Option<u32>,
}

impl fmt::Display for QueryError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{} ({})", self.id, self.msg)?;
		if let Some(extra) = &self.extra_msg {
			write!(f, ": {extra}")?;
		}
		if let Some(perm) = self.failed_permid {
			write!(f, " [failed permission {perm}]")?;
		}
		Ok(())
	}
}

impl QueryError {
	pub fn is_ok(&self) -> bool {
		self.id == 0
	}

	/// The server has no data for a list command (`database empty result set`).
	pub fn is_empty_result(&self) -> bool {
		self.id == 1281
	}

	pub(crate) fn from_row(row: &Row) -> Self {
		QueryError {
			id: row.parse("id").unwrap_or(u32::MAX),
			msg: row.get("msg").unwrap_or_default().to_string(),
			extra_msg: row.get("extra_msg").map(str::to_string),
			failed_permid: row.parse("failed_permid"),
		}
	}
}

/// An event, e.g. `notifycliententerview`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
	/// Name including the `notify` prefix.
	pub name: String,
	pub rows: Vec<Row>,
}

/// A classified line from a line-based transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
	Error(QueryError),
	Notify(Notification),
	Data(Vec<Row>),
	Empty,
}

pub fn parse_line(line: &str) -> Line {
	let line = line.trim_matches(|c| c == '\r' || c == '\n');
	if line.is_empty() {
		return Line::Empty;
	}
	if let Some(rest) = line.strip_prefix("error ") {
		let row = parse_rows(rest).into_iter().next().unwrap_or_default();
		return Line::Error(QueryError::from_row(&row));
	}
	if line.starts_with("notify") {
		let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
		return Line::Notify(Notification { name: name.to_string(), rows: parse_rows(rest) });
	}
	Line::Data(parse_rows(line))
}

/// A command to send, built with escaping applied.
///
/// ```
/// use tsc_query::Command;
/// let cmd = Command::new("sendtextmessage").arg("targetmode", 3).arg("msg", "hi there");
/// assert_eq!(cmd.to_line(), "sendtextmessage targetmode=3 msg=hi\\sthere");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
	pub name: String,
	/// Rows; the first row also carries the command-wide arguments.
	pub rows: Vec<Vec<(String, String)>>,
	pub flags: Vec<String>,
}

impl Command {
	pub fn new(name: impl Into<String>) -> Self {
		Self { name: name.into(), rows: vec![Vec::new()], flags: Vec::new() }
	}

	/// Add an argument to the current row.
	pub fn arg(mut self, key: impl Into<String>, value: impl fmt::Display) -> Self {
		self.rows.last_mut().unwrap().push((key.into(), value.to_string()));
		self
	}

	/// Start a new `|`-separated row.
	pub fn row(mut self) -> Self {
		self.rows.push(Vec::new());
		self
	}

	/// Add an option like `-uid`.
	pub fn flag(mut self, flag: impl Into<String>) -> Self {
		let flag = flag.into();
		self.flags.push(if flag.starts_with('-') { flag } else { format!("-{flag}") });
		self
	}

	/// Parse an escaped command line such as `clientlist -uid` or
	/// `clientmove clid=5 cid=2|clid=6`.
	pub fn parse(line: &str) -> Option<Self> {
		let line = line.trim();
		let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
		if name.is_empty() {
			return None;
		}
		let mut cmd = Command::new(name);
		for (i, part) in rest.split('|').enumerate() {
			if i > 0 {
				cmd = cmd.row();
			}
			for token in part.split(' ').filter(|t| !t.is_empty()) {
				if token.starts_with('-') && !token.contains('=') {
					cmd.flags.push(token.to_string());
				} else {
					let (k, v) = token.split_once('=').unwrap_or((token, ""));
					cmd = cmd.arg(k, unescape(v));
				}
			}
		}
		Some(cmd)
	}

	/// Render for line-based transports (no trailing newline).
	pub fn to_line(&self) -> String {
		let mut out = self.name.clone();
		for (i, row) in self.rows.iter().enumerate() {
			if i > 0 {
				out.push('|');
			} else if !row.is_empty() {
				out.push(' ');
			}
			let parts: Vec<String> =
				row.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
			out.push_str(&parts.join(" "));
		}
		for flag in &self.flags {
			out.push(' ');
			out.push_str(flag);
		}
		out
	}
}

impl fmt::Display for Command {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.to_line())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn escape_roundtrip() {
		let s = "a b|c\\d/e\nf\tg";
		assert_eq!(escape(s), "a\\sb\\pc\\\\d\\/e\\nf\\tg");
		assert_eq!(unescape(&escape(s)), s);
		assert_eq!(unescape("trailing\\"), "trailing\\");
		assert_eq!(unescape("\\x"), "\\x");
	}

	#[test]
	fn parses_lines() {
		assert_eq!(parse_line("\n\r"), Line::Empty);
		match parse_line("error id=1281 msg=database\\sempty\\sresult\\sset\n\r") {
			Line::Error(e) => {
				assert!(e.is_empty_result());
				assert_eq!(e.msg, "database empty result set");
			}
			other => panic!("{other:?}"),
		}
		match parse_line("notifytextmessage targetmode=3 msg=hi\\sall invokerid=5 invokername=Bob")
		{
			Line::Notify(n) => {
				assert_eq!(n.name, "notifytextmessage");
				assert_eq!(n.rows[0].get("msg"), Some("hi all"));
				assert_eq!(n.rows[0].parse::<u16>("invokerid"), Some(5));
			}
			other => panic!("{other:?}"),
		}
		match parse_line("clid=1 cid=1 client_nickname=A|clid=2 cid=3 client_nickname=B\\sC") {
			Line::Data(rows) => {
				assert_eq!(rows.len(), 2);
				assert_eq!(rows[1].get("client_nickname"), Some("B C"));
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn builds_commands() {
		let cmd = Command::new("clientlist").flag("uid").flag("-away");
		assert_eq!(cmd.to_line(), "clientlist -uid -away");
		let cmd = Command::new("clientkick").arg("clid", 1).row().arg("clid", 2).arg("reasonid", 5);
		assert_eq!(cmd.to_line(), "clientkick clid=1|clid=2 reasonid=5");
		let cmd = Command::new("login")
			.arg("client_login_name", "a b")
			.arg("client_login_password", "p|w");
		assert_eq!(cmd.to_line(), "login client_login_name=a\\sb client_login_password=p\\pw");
	}

	#[test]
	fn parses_command_lines() {
		let line = r"clientmove clid=5 cid=2 cpw=a\sb|clid=6 -continueonerror";
		let cmd = Command::parse(line).unwrap();
		assert_eq!(cmd.name, "clientmove");
		assert_eq!(cmd.rows.len(), 2);
		assert_eq!(cmd.rows[0][2], ("cpw".to_string(), "a b".to_string()));
		assert_eq!(cmd.flags, vec!["-continueonerror"]);
		assert_eq!(cmd.to_line(), line);
		assert!(Command::parse("   ").is_none());
	}

	#[test]
	fn row_helpers() {
		let mut row = parse_rows("a=1 b=0 c").remove(0);
		assert_eq!(row.flag("a"), Some(true));
		assert_eq!(row.flag("b"), Some(false));
		assert_eq!(row.get("c"), Some(""));
		row.insert("b", "7");
		row.insert("d", "x");
		assert_eq!(row.parse::<i32>("b"), Some(7));
		assert_eq!(row.get("d"), Some("x"));
	}
}
