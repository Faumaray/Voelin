//! Known signed client versions.
//!
//! `clientinit` must carry a `client_version_sign` produced by TeamSpeak's
//! private key, so a client can only claim versions from this list of captured
//! signatures (the `Versions.csv` declaration file).

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use tsclientlib::Version;

const VERSIONS_CSV: &str = include_str!("../../proto/tsproto-structs/declarations/Versions.csv");

/// The signed generic version of every platform, tsclientlib's default.
const GENERIC_VERSION: &str = "3.?.? [Build: 5680278000]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownVersion {
	pub version: String,
	pub platform: String,
	pub signature: String,
}

impl KnownVersion {
	pub fn to_version(&self) -> Result<Version> {
		let signature = BASE64_STANDARD
			.decode(&self.signature)
			.with_context(|| format!("invalid signature for {}@{}", self.platform, self.version))?;
		Ok(Version::Custom {
			platform: self.platform.clone(),
			version: self.version.clone(),
			signature,
		})
	}
}

pub fn known_versions() -> Vec<KnownVersion> {
	VERSIONS_CSV
		.lines()
		.skip(1)
		.filter(|l| !l.trim().is_empty())
		.filter_map(|l| {
			// The version string contains no commas, the signature is base64.
			let mut parts = l.splitn(3, ',');
			Some(KnownVersion {
				version: parts.next()?.to_string(),
				platform: parts.next()?.to_string(),
				signature: parts.next()?.trim().to_string(),
			})
		})
		.collect()
}

/// Resolve a `--client-version` argument.
///
/// Accepts `default` (the core native-platform default), an index into [`known_versions`],
/// or `<platform>@<version>` with the exact strings from `voelinctl versions`.
pub fn resolve(spec: &str) -> Result<Option<Version>> {
	if spec == "default" {
		return Ok(None);
	}
	let versions = known_versions();
	if let Ok(i) = spec.parse::<usize>() {
		let Some(v) = versions.get(i) else {
			bail!("version index {i} out of range (0..{})", versions.len());
		};
		return v.to_version().map(Some);
	}
	let Some((platform, version)) = spec.split_once('@') else {
		bail!("expected `default`, an index or `<platform>@<version>`, got {spec:?}");
	};
	match versions.iter().find(|v| v.platform == platform && v.version == version) {
		Some(v) => v.to_version().map(Some),
		None => bail!("unknown client version {spec:?}, see `voelinctl versions`"),
	}
}

/// The signed compatibility tuple for the native operating system.
///
/// The generic version retains the library's minimum-version compatibility.
/// Its version, platform and signature must stay together: the signature does
/// not authorize renaming the client or changing its platform.
///
/// Unsupported operating systems return an error rather than claiming another
/// platform. Product identity belongs in [`client_metadata`], not this tuple.
pub fn native_version() -> Result<Version> {
	compatibility_version(std::env::consts::OS)
}

fn compatibility_version(os: &str) -> Result<Version> {
	let platform = match os {
		"linux" => "Linux",
		"windows" => "Windows",
		"macos" => "OS X",
		"android" => "Android",
		"ios" => "iOS",
		_ => bail!("no signed compatibility version for operating system {os:?}"),
	};
	// The library's generic version: its build passes servers' minimum client
	// version, which a real (older) build such as 3.6.0 may not.
	known_versions()
		.into_iter()
		.find(|version| version.platform == platform && version.version == GENERIC_VERSION)
		.with_context(|| format!("missing signed compatibility version for {platform}"))?
		.to_version()
}

/// Truthful product identity, separate from the signed compatibility tuple.
pub fn client_metadata() -> String {
	serde_json::json!({
		"name": "Voelin",
		"version": env!("CARGO_PKG_VERSION"),
		"platform": std::env::consts::OS,
	})
	.to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn native_platform_preserves_complete_signed_tuple() {
		for (os, platform) in [
			("linux", "Linux"),
			("windows", "Windows"),
			("macos", "OS X"),
			("android", "Android"),
			("ios", "iOS"),
		] {
			let version = compatibility_version(os).unwrap();
			assert_eq!(version.get_platform(), platform);
			assert_eq!(version.get_version_string(), "3.?.? [Build: 5680278000]");
			assert_eq!(version.get_signature().len(), 64);
			let known = known_versions()
				.into_iter()
				.find(|known| {
					known.platform == platform && known.version == version.get_version_string()
				})
				.unwrap();
			assert_eq!(version, known.to_version().unwrap());
		}
		assert!(compatibility_version("freebsd").is_err());
		assert!(compatibility_version("unknown").is_err());
		match compatibility_version(std::env::consts::OS) {
			Ok(expected) => assert_eq!(native_version().unwrap(), expected),
			Err(_) => assert!(native_version().is_err()),
		}
	}

	#[test]
	fn metadata_identifies_voelin_without_changing_signed_version() {
		let metadata: serde_json::Value = serde_json::from_str(&client_metadata()).unwrap();
		assert_eq!(metadata["name"], "Voelin");
		assert_eq!(metadata["version"], env!("CARGO_PKG_VERSION"));
		assert_eq!(metadata["platform"], std::env::consts::OS);
	}

	#[test]
	fn parses_all_rows() {
		let versions = known_versions();
		assert!(versions.len() > 50);
		for v in &versions {
			let signature = BASE64_STANDARD.decode(&v.signature).unwrap();
			assert_eq!(signature.len(), 64, "{v:?}");
		}
	}

	#[test]
	fn resolves_specs() {
		assert!(resolve("default").unwrap().is_none());
		let by_index = resolve("0").unwrap().unwrap();
		let first = &known_versions()[0];
		let by_name = resolve(&format!("{}@{}", first.platform, first.version)).unwrap().unwrap();
		assert_eq!(by_index, by_name);
		assert_eq!(by_name.get_platform(), first.platform);
		assert!(resolve("Nope@1.0").is_err());
		assert!(resolve("100000").is_err());
		assert!(resolve("garbage").is_err());
	}
}
