// SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Obtaining and refreshing the Zitadel project credential from zapp.
//!
//! The operator proves who it is with its Kubernetes service account token
//! (a projected token whose audience is zapp's) and gets back a **machine
//! key** for the service account that owns the cluster's Zitadel project. The
//! key is handed to the Zitadel client through a file, because that is the
//! only way the client library accepts it: the file lives in the configured
//! key directory (memory-backed in the Helm chart) for the instant the client
//! reads it and is removed right after. From then on the key exists only in
//! process memory, where the client signs short-lived JWTs with it. Nothing
//! is persisted; a restart simply asks zapp again.

use std::{
	io::Write as _,
	sync::Arc,
	time::{Duration, Instant},
};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};
use url::Url;

use crate::{config::ZappConfig, zitadel::ProjectClient};

/// zapp's answer to `POST /v1/credentials?type=key`.
#[derive(Debug, Deserialize)]
struct CredentialsResponse {
	zitadel_url: Url,
	username: String,
	project_id: String,
	/// Zitadel's JSON key file, as an object.
	key: serde_json::Value,
	expires_at: DateTime<Utc>,
}

/// A non-success answer from zapp.
#[derive(Debug, thiserror::Error)]
#[error("zapp answered {status}: {body}")]
pub struct ZappError {
	pub status: StatusCode,
	pub body: String,
}

/// An authenticated Zitadel client for the cluster's project, valid until
/// the underlying machine key expires.
pub struct Session {
	pub zitadel: ProjectClient,
	/// ID of the Zitadel project the operator manages.
	pub project_id: String,
	/// Base URL of the Zitadel instance, ending in `/`.
	pub zitadel_url: Url,
	/// Name of the Zitadel service account acting on behalf of the cluster.
	pub username: String,
	pub issued_at: DateTime<Utc>,
	pub expires_at: DateTime<Utc>,
}

impl Session {
	/// Whether the session should be replaced: expired, or past the point of
	/// its lifetime after which a renewal is due.
	pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
		now >= renewal_time(self.issued_at, self.expires_at)
	}

	/// The Zitadel URL without trailing slash, which is the OIDC issuer.
	pub fn issuer(&self) -> String {
		self.zitadel_url.as_str().trim_end_matches('/').to_string()
	}
}

/// Renew after two thirds of the credential lifetime, so that a zapp outage
/// of up to a third of the lifetime goes unnoticed.
fn renewal_time(issued_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> DateTime<Utc> {
	let lifetime = (expires_at - issued_at).max(chrono::Duration::zero());
	issued_at + lifetime * 2 / 3
}

/// Talks to a single zapp server.
pub struct ZappClient {
	http: ClientWithMiddleware,
	config: ZappConfig,
}

impl ZappClient {
	pub fn new(config: ZappConfig) -> anyhow::Result<Self> {
		let http = reqwest_middleware::ClientBuilder::new(
			reqwest::Client::builder()
				.timeout(Duration::from_secs(config.timeout_secs))
				.build()
				.context("building HTTP client")?,
		)
		.build();
		Ok(Self { http, config })
	}

	/// Reads the service account token presented to zapp. Read on every
	/// call: in a pod, the kubelet rotates the projected token in place.
	async fn service_account_token(&self) -> anyhow::Result<String> {
		let token = tokio::fs::read_to_string(&self.config.token_file)
			.await
			.with_context(|| format!("reading token {}", self.config.token_file.display()))?;
		let token = token.trim();
		anyhow::ensure!(!token.is_empty(), "token {} is empty", self.config.token_file.display());
		Ok(token.to_string())
	}

	/// Asks zapp for a fresh machine key and opens a Zitadel session with it.
	pub async fn open_session(&self) -> anyhow::Result<Session> {
		let token = self.service_account_token().await?;
		let url = self.config.url.join("v1/credentials")?;
		let response = self
			.http
			.post(url)
			.query(&[("type", "key")])
			.bearer_auth(token)
			.send()
			.await
			.context("requesting credentials from zapp")?;
		let status = response.status();
		let body = response.text().await.context("reading zapp response")?;
		if !status.is_success() {
			return Err(ZappError { status, body }.into());
		}
		let credentials: CredentialsResponse =
			serde_json::from_str(&body).context("parsing zapp response")?;
		let issued_at = Utc::now();

		let mut zitadel_url = credentials.zitadel_url;
		if !zitadel_url.path().ends_with('/') {
			let path = format!("{}/", zitadel_url.path());
			zitadel_url.set_path(&path);
		}

		// The client library only takes the key as a file. Keep it around
		// for exactly as long as the library needs to read it.
		let zitadel = {
			let key_file = {
				let mut file = tempfile::Builder::new()
					.prefix("zitadel-key-")
					.suffix(".json")
					.permissions(private_permissions())
					.tempfile_in(&self.config.key_dir)
					.with_context(|| {
						format!("creating key file in {}", self.config.key_dir.display())
					})?;
				file.write_all(credentials.key.to_string().as_bytes())
					.context("writing key file")?;
				file.flush().context("writing key file")?;
				file
			};
			let zitadel = ProjectClient::new(
				self.http.clone(),
				zitadel_url.clone(),
				key_file.path(),
				credentials.project_id.clone(),
			)
			.await;
			// Remove the key from the filesystem before looking at the
			// result, so that a failed login leaves no key behind either.
			key_file.close().context("removing key file")?;
			zitadel.with_context(|| format!("authenticating against Zitadel at {zitadel_url}"))?
		};

		info!(
			"obtained Zitadel credential for project {} as {} (expires {})",
			credentials.project_id,
			credentials.username,
			credentials.expires_at.to_rfc3339()
		);
		Ok(Session {
			zitadel,
			project_id: credentials.project_id,
			zitadel_url,
			username: credentials.username,
			issued_at,
			expires_at: credentials.expires_at,
		})
	}
}

/// Owner-only file permissions for the transient key file.
fn private_permissions() -> std::fs::Permissions {
	use std::os::unix::fs::PermissionsExt as _;
	std::fs::Permissions::from_mode(0o600)
}

/// Hands out the current [`Session`], renewing it from zapp when it gets
/// stale or when a caller reports that Zitadel no longer accepts it.
pub struct SessionManager {
	zapp: ZappClient,
	current: Mutex<Option<Arc<Session>>>,
	/// Avoids hammering zapp when every reconcile fails: after a failed
	/// renewal, further attempts wait for this backoff.
	last_failure: Mutex<Option<Instant>>,
}

/// Minimum time between two failed renewal attempts.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);

impl SessionManager {
	pub fn new(zapp: ZappClient) -> Self {
		Self { zapp, current: Mutex::new(None), last_failure: Mutex::new(None) }
	}

	/// The current session, renewed first if stale. Concurrent callers
	/// share a single renewal.
	pub async fn session(&self) -> anyhow::Result<Arc<Session>> {
		let mut current = self.current.lock().await;
		let now = Utc::now();
		if let Some(session) = current.as_ref()
			&& !session.is_stale(now)
		{
			return Ok(session.clone());
		}
		// A stale-but-valid session is kept as fallback while zapp is
		// unreachable, since it still works until it actually expires.
		let fallback = current.clone().filter(|session| now < session.expires_at);
		match self.renew().await {
			Ok(session) => {
				let session = Arc::new(session);
				*current = Some(session.clone());
				Ok(session)
			}
			Err(e) => match fallback {
				Some(session) => {
					warn!(
						"renewing the Zitadel credential failed, keeping the current one until {}: {e:#}",
						session.expires_at.to_rfc3339()
					);
					Ok(session)
				}
				None => Err(e),
			},
		}
	}

	/// Drops `session` if it is still the current one, so that the next
	/// call to [`Self::session`] asks zapp for a new credential. Called
	/// when Zitadel rejects the session's token, e.g. after zapp's garbage
	/// collection deactivated the service account.
	pub async fn invalidate(&self, session: &Arc<Session>) {
		let mut current = self.current.lock().await;
		if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, session)) {
			warn!("Zitadel rejected the current credential; requesting a new one from zapp");
			*current = None;
		}
	}

	/// Whether a usable session is at hand, for readiness.
	pub async fn is_ready(&self) -> bool {
		let now = Utc::now();
		self.current.lock().await.as_ref().is_some_and(|session| now < session.expires_at)
	}

	async fn renew(&self) -> anyhow::Result<Session> {
		{
			let last_failure = self.last_failure.lock().await;
			if let Some(at) = *last_failure
				&& at.elapsed() < FAILURE_BACKOFF
			{
				anyhow::bail!(
					"not asking zapp again yet: the last attempt failed {} ago",
					humantime::format_duration(Duration::from_secs(at.elapsed().as_secs()))
				);
			}
		}
		match self.zapp.open_session().await {
			Ok(session) => {
				*self.last_failure.lock().await = None;
				Ok(session)
			}
			Err(e) => {
				*self.last_failure.lock().await = Some(Instant::now());
				Err(e)
			}
		}
	}
}

/// Whether an error from Zitadel means the credential is no longer accepted
/// and a new one should be requested from zapp.
pub fn is_auth_error(err: &anyhow::Error) -> bool {
	if let Some(http) = err.downcast_ref::<crate::zitadel::HttpError>() {
		return http.status == StatusCode::UNAUTHORIZED;
	}
	// The client library flattens HTTP failures into messages of the form
	// "... Response: 401 Unauthorized. Body: ...".
	err.chain().any(|cause| {
		let text = cause.to_string();
		text.contains("401 Unauthorized") || text.contains("Errors.Token.Invalid")
	})
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone as _;

	use super::*;

	#[test]
	fn renewal_is_due_after_two_thirds() {
		let issued = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().expect("valid");
		let expires = issued + chrono::Duration::hours(24);
		assert_eq!(renewal_time(issued, expires), issued + chrono::Duration::hours(16));
		// Degenerate lifetimes never underflow.
		assert_eq!(renewal_time(expires, issued), expires);
	}

	#[test]
	fn recognises_auth_errors() {
		let http: anyhow::Error =
			crate::zitadel::HttpError { status: StatusCode::UNAUTHORIZED, body: "{}".to_string() }
				.into();
		assert!(is_auth_error(&http));
		let library = anyhow::anyhow!(
			"The request resulted in error. Response: 401 Unauthorized. Body: {{}}"
		)
		.context("listing applications");
		assert!(is_auth_error(&library));
		let other: anyhow::Error =
			crate::zitadel::HttpError { status: StatusCode::NOT_FOUND, body: String::new() }.into();
		assert!(!is_auth_error(&other));
		assert!(!is_auth_error(&anyhow::anyhow!("connection refused")));
	}
}
