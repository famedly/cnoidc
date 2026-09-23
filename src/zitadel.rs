// SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zitadel management API access scoped to the cluster's project.
//!
//! The client library is used for authentication and for the calls it
//! offers; the remaining application and role endpoints are called directly
//! on the same base URL and token, because the library hides the HTTP status
//! codes the operator reacts to (404: gone, 409: already there).

use std::path::Path;

use anyhow::Context as _;
use famedly_zitadel_rust_client::v2::{
	Zitadel,
	authentication::Token,
	management::{
		ManagementServiceAddOidcAppBody, Projectv1Role, V1AddOidcAppResponse, V1App,
		V1AppNameQuery, V1AppQuery, V1AppState, V1OidcAppType, V1OidcAuthMethodType,
		V1OidcGrantType, V1OidcResponseType, V1OidcTokenType, V1OidcVersion, V1TextQueryMethod,
		Zitadelappv1OidcConfig,
	},
};
use futures::TryStreamExt as _;
use reqwest::{Method, StatusCode};
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use tracing::info;
use url::Url;

use crate::crd::{
	AccessTokenType, AppType, AuthMethod, GrantType, OIDCApplicationSpec, ResponseType,
};

/// A non-success answer from Zitadel, kept typed so callers can react to
/// specific statuses.
#[derive(Debug, thiserror::Error)]
#[error("Zitadel answered {status}: {body}")]
pub struct HttpError {
	pub status: StatusCode,
	pub body: String,
}

fn has_status(err: &anyhow::Error, status: StatusCode) -> bool {
	err.downcast_ref::<HttpError>().is_some_and(|e| e.status == status)
}

/// Desired OIDC configuration of an application, as sent to Zitadel.
#[derive(Debug, Clone)]
pub struct OidcSettings {
	pub redirect_uris: Vec<String>,
	pub post_logout_redirect_uris: Vec<String>,
	pub app_type: V1OidcAppType,
	pub auth_method_type: V1OidcAuthMethodType,
	pub grant_types: Vec<V1OidcGrantType>,
	pub response_types: Vec<V1OidcResponseType>,
	pub access_token_type: V1OidcTokenType,
	pub access_token_role_assertion: bool,
	pub id_token_role_assertion: bool,
	pub id_token_userinfo_assertion: bool,
	pub dev_mode: bool,
	pub additional_origins: Vec<String>,
	pub clock_skew: Option<String>,
	pub back_channel_logout_uri: Option<String>,
	pub skip_native_app_success_page: bool,
}

impl From<AppType> for V1OidcAppType {
	fn from(value: AppType) -> Self {
		match value {
			AppType::Web => V1OidcAppType::Web,
			AppType::UserAgent => V1OidcAppType::UserAgent,
			AppType::Native => V1OidcAppType::Native,
		}
	}
}

impl From<AuthMethod> for V1OidcAuthMethodType {
	fn from(value: AuthMethod) -> Self {
		match value {
			AuthMethod::None => V1OidcAuthMethodType::None,
			AuthMethod::Basic => V1OidcAuthMethodType::Basic,
			AuthMethod::Post => V1OidcAuthMethodType::Post,
			AuthMethod::PrivateKeyJwt => V1OidcAuthMethodType::PrivateKeyJwt,
		}
	}
}

impl From<GrantType> for V1OidcGrantType {
	fn from(value: GrantType) -> Self {
		match value {
			GrantType::AuthorizationCode => V1OidcGrantType::AuthorizationCode,
			GrantType::Implicit => V1OidcGrantType::Implicit,
			GrantType::RefreshToken => V1OidcGrantType::RefreshToken,
			GrantType::DeviceCode => V1OidcGrantType::DeviceCode,
			GrantType::TokenExchange => V1OidcGrantType::TokenExchange,
		}
	}
}

impl From<ResponseType> for V1OidcResponseType {
	fn from(value: ResponseType) -> Self {
		match value {
			ResponseType::Code => V1OidcResponseType::Code,
			ResponseType::IdToken => V1OidcResponseType::IdToken,
			ResponseType::IdTokenToken => V1OidcResponseType::IdTokenToken,
		}
	}
}

impl From<AccessTokenType> for V1OidcTokenType {
	fn from(value: AccessTokenType) -> Self {
		match value {
			AccessTokenType::Bearer => V1OidcTokenType::Bearer,
			AccessTokenType::Jwt => V1OidcTokenType::Jwt,
		}
	}
}

/// Zitadel's `google.protobuf.Duration` JSON form for a clock skew, e.g.
/// "5s"; humantime input such as "5s" or "500ms" is normalised to it.
fn clock_skew_to_zitadel(skew: &str) -> anyhow::Result<String> {
	let duration =
		humantime::parse_duration(skew).with_context(|| format!("clock skew {skew:?}"))?;
	if duration.subsec_nanos() == 0 {
		Ok(format!("{}s", duration.as_secs()))
	} else {
		Ok(format!("{}s", duration.as_secs_f64()))
	}
}

impl OidcSettings {
	pub fn from_spec(spec: &OIDCApplicationSpec) -> anyhow::Result<Self> {
		let mut grant_types: Vec<GrantType> = spec.grant_types.clone();
		grant_types.sort();
		grant_types.dedup();
		let mut response_types: Vec<ResponseType> = spec.response_types.clone();
		response_types.sort();
		response_types.dedup();
		Ok(Self {
			redirect_uris: spec.redirect_uris.clone(),
			post_logout_redirect_uris: spec.post_logout_redirect_uris.clone(),
			app_type: spec.app_type.into(),
			auth_method_type: spec.auth_method.into(),
			grant_types: grant_types.into_iter().map(Into::into).collect(),
			response_types: response_types.into_iter().map(Into::into).collect(),
			access_token_type: spec.access_token_type.into(),
			access_token_role_assertion: spec.access_token_role_assertion,
			id_token_role_assertion: spec.id_token_role_assertion,
			id_token_userinfo_assertion: spec.id_token_userinfo_assertion,
			dev_mode: spec.dev_mode,
			additional_origins: spec.additional_origins.clone(),
			clock_skew: spec.clock_skew.as_deref().map(clock_skew_to_zitadel).transpose()?,
			back_channel_logout_uri: spec.back_channel_logout_uri.clone(),
			skip_native_app_success_page: spec.skip_native_app_success_page,
		})
	}

	/// Whether `current` (as reported by Zitadel) already matches.
	///
	/// The generated Zitadel types do not implement `PartialEq`, so enums
	/// are compared by their wire representation.
	pub fn matches(&self, current: &Zitadelappv1OidcConfig) -> bool {
		fn wire<T: serde::Serialize>(value: &T) -> serde_json::Value {
			serde_json::to_value(value).unwrap_or_default()
		}
		fn same_set<T: serde::Serialize>(want: &[T], have: Option<&Vec<T>>) -> bool {
			let want: Vec<_> = want.iter().map(wire).collect();
			let have: Vec<_> = have.into_iter().flatten().map(wire).collect();
			want.len() == have.len()
				&& want.iter().all(|w| have.contains(w))
				&& have.iter().all(|h| want.contains(h))
		}
		fn same_enum<T: serde::Serialize>(want: &T, have: Option<&T>) -> bool {
			have.is_some_and(|have| wire(have) == wire(want))
		}
		fn flag(have: Option<&bool>) -> bool {
			have.copied().unwrap_or(false)
		}
		fn opt_str(have: Option<&String>) -> Option<&str> {
			have.map(String::as_str).filter(|s| !s.is_empty())
		}
		same_set(&self.redirect_uris, current.redirect_uris())
			&& same_set(&self.post_logout_redirect_uris, current.post_logout_redirect_uris())
			&& same_enum(&self.app_type, current.app_type())
			&& same_enum(&self.auth_method_type, current.auth_method_type())
			&& same_set(&self.grant_types, current.grant_types())
			&& same_set(&self.response_types, current.response_types())
			&& same_enum(&self.access_token_type, current.access_token_type())
			&& flag(current.access_token_role_assertion()) == self.access_token_role_assertion
			&& flag(current.id_token_role_assertion()) == self.id_token_role_assertion
			&& flag(current.id_token_userinfo_assertion()) == self.id_token_userinfo_assertion
			&& flag(current.dev_mode()) == self.dev_mode
			&& same_set(&self.additional_origins, current.additional_origins())
			&& opt_str(current.clock_skew()).unwrap_or("0s")
				== self.clock_skew.as_deref().unwrap_or("0s")
			&& opt_str(current.back_channel_logout_uri()) == self.back_channel_logout_uri.as_deref()
			&& flag(current.skip_native_app_success_page()) == self.skip_native_app_success_page
	}

	fn add_body(&self, name: &str) -> ManagementServiceAddOidcAppBody {
		let mut body = ManagementServiceAddOidcAppBody::new(name.to_string())
			.with_redirect_uris(self.redirect_uris.clone())
			.with_post_logout_redirect_uris(self.post_logout_redirect_uris.clone())
			.with_response_types(self.response_types.clone())
			.with_grant_types(self.grant_types.clone())
			.with_app_type(self.app_type.clone())
			.with_auth_method_type(self.auth_method_type.clone())
			.with_version(V1OidcVersion::V1_0)
			.with_dev_mode(self.dev_mode)
			.with_access_token_type(self.access_token_type.clone())
			.with_access_token_role_assertion(self.access_token_role_assertion)
			.with_id_token_role_assertion(self.id_token_role_assertion)
			.with_id_token_userinfo_assertion(self.id_token_userinfo_assertion)
			.with_additional_origins(self.additional_origins.clone())
			.with_skip_native_app_success_page(self.skip_native_app_success_page);
		if let Some(skew) = &self.clock_skew {
			body = body.with_clock_skew(skew.clone());
		}
		if let Some(uri) = &self.back_channel_logout_uri {
			body = body.with_back_channel_logout_uri(uri.clone());
		}
		body
	}

	fn update_body(&self) -> serde_json::Value {
		// Same fields as the add body minus the name; serialising the add
		// body and dropping `name` keeps the two in lockstep.
		let mut value = serde_json::to_value(self.add_body("")).unwrap_or_default();
		if let Some(object) = value.as_object_mut() {
			object.remove("name");
			object.remove("version");
		}
		value
	}
}

/// What Zitadel knows about an application after a sync.
#[derive(Debug, Clone)]
pub struct SyncedApplication {
	pub app_id: String,
	pub client_id: String,
	/// Only set when the application was just created (or its secret
	/// regenerated) and uses a client secret; Zitadel never returns it
	/// again.
	pub client_secret: Option<String>,
	/// Whether a change was pushed to Zitadel.
	pub changed: bool,
}

#[derive(Deserialize)]
struct Listing<T> {
	#[serde(default = "Vec::new")]
	result: Vec<T>,
}

/// Zitadel client acting as the project's service account.
pub struct ProjectClient {
	zitadel: Zitadel,
	/// Token holder for the requests made without the client library (which
	/// hides HTTP status codes); it authenticates with the same machine key.
	token: Token,
	http: ClientWithMiddleware,
	url: Url,
	project_id: String,
}

impl ProjectClient {
	/// Authenticates with the machine key at `key_file`, which the caller
	/// removes afterwards; the key stays in memory only.
	pub async fn new(
		http: ClientWithMiddleware,
		url: Url,
		key_file: &Path,
		project_id: String,
	) -> anyhow::Result<Self> {
		let zitadel = Zitadel::new(http.clone(), url.clone(), key_file.to_path_buf(), None)
			.await
			.context("authenticating against Zitadel")?;
		let token = Token::new(url.clone(), &key_file.to_path_buf(), http.clone(), None, None)
			.await
			.context("authenticating against Zitadel")?;
		Ok(Self { zitadel, token, http, url, project_id })
	}

	// --- Applications

	/// Finds the OIDC application named `name` in the project, if any.
	pub async fn find_application(&self, name: &str) -> anyhow::Result<Option<V1App>> {
		let query = V1AppQuery::new().with_name_query(
			V1AppNameQuery::new()
				.with_name(name.to_string())
				.with_method(V1TextQueryMethod::Equals),
		);
		let apps: Vec<V1App> = self
			.zitadel
			.list_applications(self.project_id.clone(), None, None, Some(vec![query]))
			.context("listing applications")?
			.try_collect()
			.await
			.context("listing applications")?;
		// Exact-match query, but never adopt anything not asked for, and
		// only OIDC applications (an API or SAML app of that name is not
		// ours to touch).
		Ok(apps.into_iter().find(|app| {
			app.name().map(String::as_str) == Some(name) && app.oidc_config().is_some()
		}))
	}

	/// Looks up an application by ID; `None` if it no longer exists.
	pub async fn get_application(&self, app_id: &str) -> anyhow::Result<Option<V1App>> {
		#[derive(Deserialize)]
		struct Response {
			app: Option<V1App>,
		}
		match self
			.call::<Response>(
				Method::GET,
				&format!("projects/{}/apps/{app_id}", self.project_id),
				None,
			)
			.await
		{
			Ok(response) => Ok(response.app),
			Err(e) if has_status(&e, StatusCode::NOT_FOUND) => Ok(None),
			Err(e) => Err(e.context(format!("looking up application {app_id}"))),
		}
	}

	/// Ensures an OIDC application `name` with `settings` exists in the
	/// project: created if missing, otherwise adopted by name (or found by
	/// `known_id` if the resource remembers one) and updated on drift.
	pub async fn sync_application(
		&self,
		name: &str,
		known_id: Option<&str>,
		settings: &OidcSettings,
	) -> anyhow::Result<SyncedApplication> {
		let existing = match known_id {
			Some(id) => match self.get_application(id).await? {
				Some(app) if app.oidc_config().is_some() => Some(app),
				_ => self.find_application(name).await?,
			},
			None => self.find_application(name).await?,
		};

		let Some(app) = existing else {
			let created: V1AddOidcAppResponse = self
				.call(
					Method::POST,
					&format!("projects/{}/apps/oidc", self.project_id),
					Some(serde_json::to_value(settings.add_body(name))?),
				)
				.await
				.context("creating application")?;
			let app_id = created.app_id().cloned().context("Zitadel returned no app id")?;
			let client_id =
				created.client_id().cloned().context("Zitadel returned no client id")?;
			info!("created application {name} ({app_id}) with client id {client_id}");
			return Ok(SyncedApplication {
				app_id,
				client_id,
				client_secret: created.client_secret().cloned().filter(|s| !s.is_empty()),
				changed: true,
			});
		};

		let app_id = app.id().cloned().context("Zitadel returned an application without id")?;
		let config = app.oidc_config().context("application has no OIDC config")?;
		let client_id = config.client_id().cloned().context("Zitadel returned no client id")?;
		let mut changed = false;

		if matches!(app.state(), Some(V1AppState::AppStateInactive)) {
			self.call::<serde::de::IgnoredAny>(
				Method::POST,
				&format!("projects/{}/apps/{app_id}/_reactivate", self.project_id),
				None,
			)
			.await
			.context("reactivating application")?;
			info!("reactivated application {name} ({app_id})");
			changed = true;
		}

		if app.name().map(String::as_str) != Some(name) {
			self.call::<serde::de::IgnoredAny>(
				Method::PUT,
				&format!("projects/{}/apps/{app_id}", self.project_id),
				Some(json!({ "name": name })),
			)
			.await
			.context("renaming application")?;
			info!("renamed application {app_id} to {name}");
			changed = true;
		}

		let mut client_secret = None;
		if !settings.matches(config) {
			let method_changed = serde_json::to_value(config.auth_method_type()).ok()
				!= serde_json::to_value(Some(&settings.auth_method_type)).ok();
			self.call::<serde::de::IgnoredAny>(
				Method::PUT,
				&format!("projects/{}/apps/{app_id}/oidc_config", self.project_id),
				Some(settings.update_body()),
			)
			.await
			.context("updating application configuration")?;
			info!("updated application {name} ({app_id})");
			changed = true;
			// Switching to a secret-based method makes Zitadel generate a
			// secret that is not returned by the update; fetch a fresh one.
			if method_changed
				&& matches!(
					settings.auth_method_type,
					V1OidcAuthMethodType::Basic | V1OidcAuthMethodType::Post
				) {
				client_secret = Some(self.regenerate_client_secret(&app_id).await?);
			}
		}

		Ok(SyncedApplication { app_id, client_id, client_secret, changed })
	}

	/// Generates a new client secret for the application; the old one stops
	/// working.
	pub async fn regenerate_client_secret(&self, app_id: &str) -> anyhow::Result<String> {
		#[derive(Deserialize)]
		struct Response {
			#[serde(rename = "clientSecret")]
			client_secret: String,
		}
		let response: Response = self
			.call(
				Method::POST,
				&format!(
					"projects/{}/apps/{app_id}/oidc_config/_generate_client_secret",
					self.project_id
				),
				None,
			)
			.await
			.context("regenerating client secret")?;
		info!("regenerated client secret of application {app_id}");
		Ok(response.client_secret)
	}

	/// Removes the application; one that is already gone is fine.
	pub async fn remove_application(&self, app_id: &str) -> anyhow::Result<()> {
		match self
			.call::<serde::de::IgnoredAny>(
				Method::DELETE,
				&format!("projects/{}/apps/{app_id}", self.project_id),
				None,
			)
			.await
		{
			Ok(_) => {
				info!("removed application {app_id}");
				Ok(())
			}
			Err(e) if has_status(&e, StatusCode::NOT_FOUND) => Ok(()),
			Err(e) => Err(e.context(format!("removing application {app_id}"))),
		}
	}

	// --- Roles

	/// Finds the project role with `key`, if any.
	pub async fn find_role(&self, key: &str) -> anyhow::Result<Option<Projectv1Role>> {
		let roles: Listing<Projectv1Role> = self
			.call(
				Method::POST,
				&format!("projects/{}/roles/_search", self.project_id),
				Some(json!({
					"query": { "limit": 100 },
					"queries": [{ "keyQuery": { "key": key, "method": "TEXT_QUERY_METHOD_EQUALS" } }],
				})),
			)
			.await
			.context("listing roles")?;
		Ok(roles.result.into_iter().find(|role| role.key().map(String::as_str) == Some(key)))
	}

	/// Ensures a role `key` with the given display name and group exists.
	/// Returns whether Zitadel was changed.
	pub async fn sync_role(
		&self,
		key: &str,
		display_name: &str,
		group: Option<&str>,
	) -> anyhow::Result<bool> {
		let Some(role) = self.find_role(key).await? else {
			match self
				.call::<serde::de::IgnoredAny>(
					Method::POST,
					&format!("projects/{}/roles", self.project_id),
					Some(json!({ "roleKey": key, "displayName": display_name, "group": group })),
				)
				.await
			{
				Ok(_) => {
					info!("created role {key}");
					return Ok(true);
				}
				// Lost a race with a concurrent creation: fall through to
				// the update path on the next reconcile.
				Err(e) if has_status(&e, StatusCode::CONFLICT) => return Ok(false),
				Err(e) => return Err(e.context("creating role")),
			}
		};

		let same_display = role.display_name().map(String::as_str) == Some(display_name);
		let same_group = role.group().map(String::as_str).filter(|g| !g.is_empty()) == group;
		if same_display && same_group {
			return Ok(false);
		}
		self.call::<serde::de::IgnoredAny>(
			Method::PUT,
			&format!("projects/{}/roles/{key}", self.project_id),
			Some(json!({ "displayName": display_name, "group": group })),
		)
		.await
		.context("updating role")?;
		info!("updated role {key}");
		Ok(true)
	}

	/// Removes the role; one that is already gone is fine.
	pub async fn remove_role(&self, key: &str) -> anyhow::Result<()> {
		match self
			.call::<serde::de::IgnoredAny>(
				Method::DELETE,
				&format!("projects/{}/roles/{key}", self.project_id),
				None,
			)
			.await
		{
			Ok(_) => {
				info!("removed role {key}");
				Ok(())
			}
			Err(e) if has_status(&e, StatusCode::NOT_FOUND) => Ok(()),
			Err(e) => Err(e.context(format!("removing role {key}"))),
		}
	}

	// --- Transport

	/// Sends a management API v1 request to `path` (relative to
	/// `management/v1/`) and parses the response. Non-success answers
	/// surface as [`HttpError`].
	async fn call<T: DeserializeOwned>(
		&self,
		method: Method,
		path: &str,
		body: Option<serde_json::Value>,
	) -> anyhow::Result<T> {
		let url = self.url.join(&format!("management/v1/{path}"))?;
		let token = self.token.token().await.context("refreshing Zitadel access token")?;
		let mut request =
			self.http.request(method, url).bearer_auth(token).header("Accept", "application/json");
		if let Some(body) = &body {
			request = request.json(body);
		}
		let response = request.send().await?;
		let status = response.status();
		let text = response.text().await?;
		if !status.is_success() {
			return Err(HttpError { status, body: text }.into());
		}
		let text = if text.trim().is_empty() { "null" } else { text.as_str() };
		serde_json::from_str(text).context("parsing Zitadel response")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clock_skew_is_normalised() {
		assert_eq!(clock_skew_to_zitadel("5s").expect("valid"), "5s");
		assert_eq!(clock_skew_to_zitadel("1m").expect("valid"), "60s");
		assert_eq!(clock_skew_to_zitadel("500ms").expect("valid"), "0.5s");
		assert!(clock_skew_to_zitadel("soon").is_err());
	}

	#[test]
	fn settings_match_ignores_order() {
		let spec: OIDCApplicationSpec = serde_json::from_value(json!({
			"redirectUris": ["https://a/cb", "https://b/cb"],
			"grantTypes": ["refreshToken", "authorizationCode"],
		}))
		.expect("valid spec");
		let settings = OidcSettings::from_spec(&spec).expect("valid settings");
		let current = Zitadelappv1OidcConfig::new()
			.with_redirect_uris(vec!["https://b/cb".into(), "https://a/cb".into()])
			.with_post_logout_redirect_uris(vec![])
			.with_app_type(V1OidcAppType::Web)
			.with_auth_method_type(V1OidcAuthMethodType::None)
			.with_grant_types(vec![
				V1OidcGrantType::AuthorizationCode,
				V1OidcGrantType::RefreshToken,
			])
			.with_response_types(vec![V1OidcResponseType::Code])
			.with_access_token_type(V1OidcTokenType::Bearer)
			.with_clock_skew("0s".into());
		assert!(settings.matches(&current));
		let drifted = current.clone().with_dev_mode(true);
		assert!(!settings.matches(&drifted));
		let drifted = current.with_redirect_uris(vec!["https://a/cb".into()]);
		assert!(!settings.matches(&drifted));
	}

	#[test]
	fn update_body_has_no_name() {
		let spec: OIDCApplicationSpec =
			serde_json::from_value(json!({ "redirectUris": ["https://a/cb"] })).expect("valid");
		let settings = OidcSettings::from_spec(&spec).expect("valid settings");
		let body = settings.update_body();
		assert!(body.get("name").is_none());
		assert_eq!(body["redirectUris"], json!(["https://a/cb"]));
		assert_eq!(body["grantTypes"], json!(["OIDC_GRANT_TYPE_AUTHORIZATION_CODE"]));
		assert_eq!(body["authMethodType"], json!("OIDC_AUTH_METHOD_TYPE_NONE"));
	}
}
