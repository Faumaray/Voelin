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
/// The client generation claimed where a signed build of it exists.
const CLIENT_GENERATION: &str = "6.";

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
/// Accepts `default` (the core native-platform default), `generic` (the
/// generic `3.?.?` version of the native platform, as before 6.x builds were
/// claimed), an index into [`known_versions`], or `<platform>@<version>` with
/// the exact strings from `voelinctl versions`.
pub fn resolve(spec: &str) -> Result<Option<Version>> {
	if spec == "default" {
		return Ok(None);
	}
	if spec == "generic" {
		return generic_version(std::env::consts::OS).map(Some);
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

/// The signed compatibility tuple for the native operating system: the
/// newest TeamSpeak 6 client build signed for it, so servers and other
/// clients see a 6.x client (its build is recent enough for any server's
/// minimum client version); where none is signed for the platform, the
/// generic version, whose build passes every minimum.
///
/// Version, platform and signature must stay together: the signature does
/// not authorize renaming the client or changing its platform.
///
/// Unsupported operating systems return an error rather than claiming another
/// platform. Product identity belongs in [`client_metadata`], not this tuple.
pub fn native_version() -> Result<Version> {
	compatibility_version(std::env::consts::OS)
}

fn compatibility_version(os: &str) -> Result<Version> {
	// TeamSpeak 5 and 6 call macOS "macOS", TeamSpeak 3 "OS X".
	let platforms: &[&str] = match os {
		"linux" => &["Linux"],
		"windows" => &["Windows"],
		"macos" => &["macOS", "OS X"],
		"android" => &["Android"],
		"ios" => &["iOS"],
		_ => bail!("no signed compatibility version for operating system {os:?}"),
	};
	let versions = known_versions();
	let newest = versions
		.iter()
		.filter(|v| platforms.contains(&v.platform.as_str()))
		.filter(|v| v.version.starts_with(CLIENT_GENERATION))
		.filter_map(|v| Some((build(&v.version)?, v)))
		.max_by_key(|(build, _)| *build)
		.map(|(_, v)| v);
	match newest {
		Some(version) => version.to_version(),
		None => generic_version(os),
	}
}

/// The generic version of the operating system's platform.
fn generic_version(os: &str) -> Result<Version> {
	let platform = match os {
		"linux" => "Linux",
		"windows" => "Windows",
		"macos" => "OS X",
		"android" => "Android",
		"ios" => "iOS",
		_ => bail!("no signed compatibility version for operating system {os:?}"),
	};
	known_versions()
		.into_iter()
		.find(|v| v.platform == platform && v.version == GENERIC_VERSION)
		.with_context(|| format!("missing signed compatibility version for {platform}"))?
		.to_version()
}

/// The build number of a version string (`6.0.0 [Build: 1737468425]`).
fn build(version: &str) -> Option<u64> {
	version.split_once("[Build: ")?.1.strip_suffix(']')?.parse().ok()
}

/// Truthful product identity, separate from the signed compatibility tuple.
pub fn client_metadata() -> String {
	serde_json::json!({
		"name": "Voelin",
		// The app's version (it sets it at start), not this library's.
		"version": voelin_platform::crash::app_version(),
		"platform": std::env::consts::OS,
	})
	.to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn native_platform_preserves_complete_signed_tuple() {
		for (os, platform, version) in [
			// The newest TeamSpeak 6 build signed for the platform.
			("linux", "Linux", "6.0.0-beta4.1 [Build: 1779880475]"),
			("windows", "Windows", "6.0.0-beta2 [Build: 1737468425]"),
			// None signed: the generic version.
			("macos", "OS X", GENERIC_VERSION),
			("android", "Android", GENERIC_VERSION),
			("ios", "iOS", GENERIC_VERSION),
		] {
			let expected = version;
			let version = compatibility_version(os).unwrap();
			assert_eq!(version.get_platform(), platform);
			assert_eq!(version.get_version_string(), expected);
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
		assert_eq!(metadata["version"], voelin_platform::crash::app_version());
		assert_eq!(metadata["platform"], std::env::consts::OS);
	}

	#[test]
	fn reads_build_numbers() {
		assert_eq!(build("6.0.0-beta2 [Build: 1737468425]"), Some(1_737_468_425));
		assert_eq!(build(GENERIC_VERSION), Some(5_680_278_000));
		assert_eq!(build("3.0.0 [Build: x]"), None);
		assert_eq!(build("6.0.0"), None);
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
		if let Ok(generic) = generic_version(std::env::consts::OS) {
			assert_eq!(resolve("generic").unwrap(), Some(generic.clone()));
			assert_eq!(generic.get_version_string(), GENERIC_VERSION);
		}
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
