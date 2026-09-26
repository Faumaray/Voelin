//! Known signed client versions.
//!
//! `clientinit` must carry a `client_version_sign` produced by TeamSpeak's
//! private key, so a client can only claim versions from this list of captured
//! signatures (the `Versions.csv` declaration file).

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use tsclientlib::Version;

const VERSIONS_CSV: &str =
	include_str!("../../../crates/proto/tsproto-structs/declarations/Versions.csv");

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
/// Accepts `default` (the library default), an index into [`known_versions`],
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

#[cfg(test)]
mod tests {
	use super::*;

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
