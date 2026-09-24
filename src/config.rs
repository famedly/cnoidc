// SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
//
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{path::PathBuf, time::Duration};

use anyhow::Context;
use serde::Deserialize;
use url::Url;

#[derive(Debug, Deserialize)]
pub struct Config {
	/// Address the health endpoint listens on, e.g. "0.0.0.0:8080".
	#[serde(default = "default_listen")]
	pub listen: String,
	/// Namespace whose `OIDCApplication` and `ProjectRole` resources are
	/// managed. All namespaces when unset.
	#[serde(default)]
	pub namespace: Option<String>,
	/// How often every resource is re-synced with Zitadel even without a
	/// change, e.g. "10m". Drift in Zitadel is repaired on that cadence.
	#[serde(default = "default_sync_interval", with = "humantime_serde")]
	pub sync_interval: Duration,
	pub zapp: ZappConfig,
}

#[derive(Debug, Deserialize)]
pub struct ZappConfig {
	/// Base URL of the zapp server that hands out the Zitadel project
	/// credential, e.g. "https://zapp.example.com".
	pub url: Url,
	/// Path to the Kubernetes service account token presented to zapp. In a
	/// pod this is a projected volume whose audience matches zapp's
	/// `verification.audience`; the kubelet keeps it fresh.
	#[serde(default = "default_token_file")]
	pub token_file: PathBuf,
	/// Directory the Zitadel machine key received from zapp is written to
	/// for the moment the Zitadel client reads it; the file is removed right
	/// after. Point this at memory-backed storage (a `tmpfs`) so the key
	/// never touches a disk.
	#[serde(default = "default_key_dir")]
	pub key_dir: PathBuf,
	/// Timeout in seconds for requests to zapp.
	#[serde(default = "default_timeout_secs")]
	pub timeout_secs: u64,
}

impl Config {
	pub fn load(path: &str) -> anyhow::Result<Self> {
		let raw =
			std::fs::read_to_string(path).with_context(|| format!("reading config {path}"))?;
		let config: Config = toml::from_str(&raw).with_context(|| format!("parsing {path}"))?;
		config.validated()
	}

	fn validated(mut self) -> anyhow::Result<Self> {
		// Paths are joined onto the base URL, so it must denote a directory.
		if !self.zapp.url.path().ends_with('/') {
			let path = format!("{}/", self.zapp.url.path());
			self.zapp.url.set_path(&path);
		}
		if !matches!(self.zapp.url.scheme(), "http" | "https") {
			anyhow::bail!("zapp.url must be an http(s) URL");
		}
		if self.sync_interval.is_zero() {
			anyhow::bail!("sync_interval must be positive");
		}
		if self.zapp.timeout_secs == 0 {
			anyhow::bail!("zapp.timeout_secs must be positive");
		}
		if self.namespace.as_deref().is_some_and(str::is_empty) {
			anyhow::bail!("namespace must not be empty; omit it to watch all namespaces");
		}
		Ok(self)
	}
}

fn default_listen() -> String {
	"127.0.0.1:8080".to_string()
}

fn default_sync_interval() -> Duration {
	Duration::from_secs(10 * 60)
}

fn default_token_file() -> PathBuf {
	PathBuf::from("/var/run/secrets/cnoidc/token")
}

fn default_key_dir() -> PathBuf {
	std::env::temp_dir()
}

fn default_timeout_secs() -> u64 {
	10
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(toml: &str) -> anyhow::Result<Config> {
		toml::from_str::<Config>(toml).map_err(anyhow::Error::from)?.validated()
	}

	const MINIMAL: &str = r#"
[zapp]
url = "https://zapp.example.com"
"#;

	#[test]
	fn accepts_minimal_config() {
		let config = parse(MINIMAL).expect("valid config");
		assert_eq!(config.listen, "127.0.0.1:8080");
		assert_eq!(config.namespace, None);
		assert_eq!(config.sync_interval, Duration::from_secs(600));
		assert_eq!(config.zapp.url.as_str(), "https://zapp.example.com/");
		assert_eq!(config.zapp.token_file, PathBuf::from("/var/run/secrets/cnoidc/token"));
		assert_eq!(config.zapp.timeout_secs, 10);
	}

	#[test]
	fn normalizes_url_to_directory() {
		let toml = MINIMAL.replace("https://zapp.example.com", "https://example.com/zapp");
		let config = parse(&toml).expect("valid config");
		assert_eq!(config.zapp.url.as_str(), "https://example.com/zapp/");
	}

	#[test]
	fn parses_humantime_and_namespace() {
		let toml = format!("namespace = \"apps\"\nsync_interval = \"1h 30m\"\n{MINIMAL}");
		let config = parse(&toml).expect("valid config");
		assert_eq!(config.namespace.as_deref(), Some("apps"));
		assert_eq!(config.sync_interval, Duration::from_secs(90 * 60));
	}

	#[test]
	fn rejects_invalid_values() {
		assert!(parse(&format!("sync_interval = \"0s\"\n{MINIMAL}")).is_err());
		assert!(parse(&format!("namespace = \"\"\n{MINIMAL}")).is_err());
		assert!(parse(&MINIMAL.replace("https://", "ftp://")).is_err());
		assert!(parse(&format!("{MINIMAL}timeout_secs = 0\n")).is_err());
	}
}
