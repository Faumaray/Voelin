//! Server software detection and the capabilities that follow from it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Parsed `virtualserver_version`, e.g. `3.13.8 [Build: 1779874471]` or
/// `6.0.0-beta13.1 [Build: 1790080330]`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerVersion {
	pub major: u32,
	pub minor: u32,
	pub patch: u32,
	/// Pre-release tag such as `beta13.1`.
	pub pre: Option<String>,
	/// Build timestamp (seconds since the Unix epoch).
	pub build: Option<u64>,
}

impl ServerVersion {
	pub fn parse(s: &str) -> Option<Self> {
		let s = s.trim();
		let (version, rest) = s.split_once(' ').unwrap_or((s, ""));
		let build = rest
			.trim()
			.strip_prefix("[Build:")
			.and_then(|b| b.strip_suffix(']'))
			.and_then(|b| b.trim().parse().ok());
		let (numbers, pre) = match version.split_once('-') {
			Some((n, p)) => (n, Some(p.to_string())),
			None => (version, None),
		};
		let mut parts = numbers.split('.').map(|p| p.parse::<u32>());
		let major = parts.next()?.ok()?;
		let minor = parts.next().unwrap_or(Ok(0)).ok()?;
		let patch = parts.next().unwrap_or(Ok(0)).ok()?;
		Some(Self { major, minor, patch, pre, build })
	}
}

impl fmt::Display for ServerVersion {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
		if let Some(pre) = &self.pre {
			write!(f, "-{pre}")?;
		}
		Ok(())
	}
}

/// Which server software we are talking to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServerFlavor {
	/// TeamSpeak 3 server (3.x).
	Ts3(ServerVersion),
	/// TeamSpeak 6 server (6.x; TeamSpeak 5 server betas are treated alike).
	Ts6(ServerVersion),
	/// Unparseable or unexpected version string.
	Unknown(String),
}

impl ServerFlavor {
	/// Classify a `virtualserver_version` string.
	pub fn from_version_string(s: &str) -> Self {
		match ServerVersion::parse(s) {
			Some(v) if v.major == 3 => ServerFlavor::Ts3(v),
			Some(v) if v.major >= 5 => ServerFlavor::Ts6(v),
			_ => ServerFlavor::Unknown(s.to_string()),
		}
	}

	pub fn version(&self) -> Option<&ServerVersion> {
		match self {
			ServerFlavor::Ts3(v) | ServerFlavor::Ts6(v) => Some(v),
			ServerFlavor::Unknown(_) => None,
		}
	}

	pub fn capabilities(&self) -> Capabilities {
		match self {
			ServerFlavor::Ts3(_) => {
				Capabilities { streams: false, raw_query: true, ssh_query: true, http_query: false }
			}
			ServerFlavor::Ts6(_) => {
				Capabilities { streams: true, raw_query: false, ssh_query: true, http_query: true }
			}
			ServerFlavor::Unknown(_) => Capabilities::default(),
		}
	}
}

/// Features that depend on the server software.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capabilities {
	/// Screen sharing / video streams (TeamSpeak 6).
	pub streams: bool,
	/// Raw TCP ServerQuery (TeamSpeak 3 only; may still be disabled by the admin).
	pub raw_query: bool,
	/// SSH ServerQuery.
	pub ssh_query: bool,
	/// HTTP WebQuery (TeamSpeak 6), request/response only, no events.
	pub http_query: bool,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_ts3() {
		let v = ServerVersion::parse("3.13.8 [Build: 1779874471]").unwrap();
		assert_eq!((v.major, v.minor, v.patch), (3, 13, 8));
		assert_eq!(v.pre, None);
		assert_eq!(v.build, Some(1_779_874_471));
		let flavor = ServerFlavor::from_version_string("3.13.8 [Build: 1779874471]");
		assert!(matches!(flavor, ServerFlavor::Ts3(_)));
		assert!(!flavor.capabilities().streams);
	}

	#[test]
	fn parses_ts6_beta() {
		let s = "6.0.0-beta13.1 [Build: 1790080330]";
		let v = ServerVersion::parse(s).unwrap();
		assert_eq!(v.pre.as_deref(), Some("beta13.1"));
		assert_eq!(v.to_string(), "6.0.0-beta13.1");
		let flavor = ServerFlavor::from_version_string(s);
		assert!(matches!(flavor, ServerFlavor::Ts6(_)));
		assert!(flavor.capabilities().streams);
		assert!(!flavor.capabilities().raw_query);
	}

	#[test]
	fn handles_garbage() {
		assert!(matches!(ServerFlavor::from_version_string(""), ServerFlavor::Unknown(_)));
		assert!(matches!(ServerFlavor::from_version_string("x.y"), ServerFlavor::Unknown(_)));
		assert_eq!(ServerVersion::parse("3").unwrap().minor, 0);
	}
}
