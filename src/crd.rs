// SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Custom resource definitions. Both kinds live in the API group
//! `cnoidc.famedly.com/v1alpha1` and are namespaced.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, CustomResourceExt as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Finalizer the operator puts on its resources so that the Zitadel side is
/// cleaned up before Kubernetes forgets about them.
pub const FINALIZER: &str = "cnoidc.famedly.com/cleanup";

/// Type of the single condition the operator maintains on its resources.
pub const CONDITION_READY: &str = "Ready";

/// An OIDC application in the cluster's Zitadel project.
///
/// The operator creates (or adopts, by name) the application in Zitadel,
/// keeps its configuration in sync with this spec and publishes the client
/// ID, project ID and Zitadel URL in a ConfigMap next to the resource. For
/// confidential clients (`authMethod: basic` or `post`) the client secret is
/// placed in a Secret.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
	group = "cnoidc.famedly.com",
	version = "v1alpha1",
	kind = "OIDCApplication",
	namespaced,
	status = "OIDCApplicationStatus",
	shortname = "oidcapp",
	doc = "An OIDC application in the cluster's Zitadel project",
	printcolumn = r#"{"name":"Client ID","type":"string","jsonPath":".status.clientId"}"#,
	printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
	printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OIDCApplicationSpec {
	/// Allowed redirect URIs after login.
	#[serde(default)]
	pub redirect_uris: Vec<String>,
	/// Allowed redirect URIs after logout.
	#[serde(default)]
	pub post_logout_redirect_uris: Vec<String>,
	/// Kind of client: `web` (server-side, may hold a secret), `userAgent`
	/// (browser SPA) or `native` (mobile/desktop).
	#[serde(default)]
	pub app_type: AppType,
	/// How the client authenticates at the token endpoint. `none` means
	/// public client with PKCE (the default); `basic` and `post` get a client
	/// secret, `privateKeyJwt` uses a key registered on the application.
	#[serde(default)]
	pub auth_method: AuthMethod,
	/// OAuth 2.0 grant types the client may use. Defaults to
	/// `[authorizationCode]`.
	#[serde(default = "default_grant_types")]
	pub grant_types: Vec<GrantType>,
	/// OAuth 2.0 response types the client may request. Defaults to `[code]`.
	#[serde(default = "default_response_types")]
	pub response_types: Vec<ResponseType>,
	/// Format of the access tokens issued to this client.
	#[serde(default)]
	pub access_token_type: AccessTokenType,
	/// Put the user's project roles into the access token.
	#[serde(default)]
	pub access_token_role_assertion: bool,
	/// Put the user's project roles into the ID token.
	#[serde(default)]
	pub id_token_role_assertion: bool,
	/// Put the user info claims (profile, email, ...) into the ID token.
	#[serde(default)]
	pub id_token_userinfo_assertion: bool,
	/// Allow `http://` redirect URIs and skip other production checks. Only
	/// for development.
	#[serde(default)]
	pub dev_mode: bool,
	/// Additional origins (scheme://host[:port]) allowed for CORS, on top of
	/// those derived from the redirect URIs.
	#[serde(default)]
	pub additional_origins: Vec<String>,
	/// Clock skew tolerated when validating tokens, e.g. `5s`. Zitadel allows
	/// at most `5s`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub clock_skew: Option<String>,
	/// URI Zitadel calls for back-channel logout.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub back_channel_logout_uri: Option<String>,
	/// Skip the success page shown to native apps after login.
	#[serde(default)]
	pub skip_native_app_success_page: bool,
	/// Name of the ConfigMap the operator writes the client ID and related
	/// settings to. Defaults to `<name>-oidc`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub config_map_name: Option<String>,
	/// Name of the Secret the client secret is written to, for `authMethod:
	/// basic` or `post`. Defaults to `<name>-oidc`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub secret_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AppType {
	#[default]
	Web,
	UserAgent,
	Native,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AuthMethod {
	#[default]
	None,
	Basic,
	Post,
	PrivateKeyJwt,
}

impl AuthMethod {
	/// Whether Zitadel issues a client secret for this method.
	pub fn uses_client_secret(self) -> bool {
		matches!(self, AuthMethod::Basic | AuthMethod::Post)
	}
}

#[derive(
	Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum GrantType {
	AuthorizationCode,
	Implicit,
	RefreshToken,
	DeviceCode,
	TokenExchange,
}

#[derive(
	Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum ResponseType {
	Code,
	IdToken,
	IdTokenToken,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AccessTokenType {
	#[default]
	Bearer,
	Jwt,
}

fn default_grant_types() -> Vec<GrantType> {
	vec![GrantType::AuthorizationCode]
}

fn default_response_types() -> Vec<ResponseType> {
	vec![ResponseType::Code]
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OIDCApplicationStatus {
	/// Generation of the spec the status reflects.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub observed_generation: Option<i64>,
	/// ID of the application in Zitadel.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub application_id: Option<String>,
	/// OIDC client ID assigned by Zitadel.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_id: Option<String>,
	/// ID of the Zitadel project the application lives in.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub project_id: Option<String>,
	/// Name of the ConfigMap the settings were written to.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub config_map: Option<String>,
	/// Name of the Secret holding the client secret, if any.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub secret: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub conditions: Vec<Condition>,
}

/// A role in the cluster's Zitadel project.
///
/// Zitadel identifies roles by their key; the operator creates the role and
/// publishes the key together with the project ID, Zitadel URL and the name
/// of the token claim carrying project roles in a ConfigMap. Keys must be
/// unique across the cluster.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
	group = "cnoidc.famedly.com",
	version = "v1alpha1",
	kind = "ProjectRole",
	namespaced,
	status = "ProjectRoleStatus",
	shortname = "zrole",
	doc = "A role in the cluster's Zitadel project",
	printcolumn = r#"{"name":"Key","type":"string","jsonPath":".status.key"}"#,
	printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
	printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoleSpec {
	/// Role key, the identifier Zitadel puts into tokens. Defaults to the
	/// resource name. Immutable once the role exists.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub key: Option<String>,
	/// Human-readable name shown in the Zitadel console. Defaults to the key.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub display_name: Option<String>,
	/// Optional group the role is sorted into in the console.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub group: Option<String>,
	/// Name of the ConfigMap the operator writes the role key and related
	/// settings to. Defaults to `<name>-role`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub config_map_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoleStatus {
	/// Generation of the spec the status reflects.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub observed_generation: Option<i64>,
	/// Key of the role in Zitadel.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub key: Option<String>,
	/// ID of the Zitadel project the role belongs to.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub project_id: Option<String>,
	/// Name of the ConfigMap the settings were written to.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub config_map: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub conditions: Vec<Condition>,
}

/// The CRDs installed by the Helm chart, in a stable order.
pub fn definitions() -> Vec<CustomResourceDefinition> {
	vec![OIDCApplication::crd(), ProjectRole::crd()]
}

/// The CRDs as a multi-document YAML string, as written to
/// `charts/cnoidc/crds/`.
pub fn definitions_yaml() -> anyhow::Result<String> {
	let mut out = String::new();
	for crd in definitions() {
		out.push_str("---\n");
		out.push_str(&serde_yaml_ng::to_string(&crd)?);
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn spec_defaults() {
		let spec: OIDCApplicationSpec =
			serde_json::from_value(serde_json::json!({ "redirectUris": ["https://a/cb"] }))
				.expect("valid spec");
		assert_eq!(spec.app_type, AppType::Web);
		assert_eq!(spec.auth_method, AuthMethod::None);
		assert_eq!(spec.grant_types, vec![GrantType::AuthorizationCode]);
		assert_eq!(spec.response_types, vec![ResponseType::Code]);
		assert_eq!(spec.access_token_type, AccessTokenType::Bearer);
		assert!(!spec.dev_mode);
	}

	#[test]
	fn enums_are_camel_case() {
		assert_eq!(
			serde_json::to_value(AuthMethod::PrivateKeyJwt).expect("serializes"),
			serde_json::json!("privateKeyJwt")
		);
		assert_eq!(
			serde_json::to_value(GrantType::AuthorizationCode).expect("serializes"),
			serde_json::json!("authorizationCode")
		);
	}

	#[test]
	fn crds_have_status_subresource_and_schema() {
		let yaml = definitions_yaml().expect("crds render");
		assert!(yaml.contains("name: oidcapplications.cnoidc.famedly.com"));
		assert!(yaml.contains("name: projectroles.cnoidc.famedly.com"));
		for crd in definitions() {
			let version = &crd.spec.versions[0];
			assert!(version.subresources.as_ref().and_then(|s| s.status.as_ref()).is_some());
			assert!(version.schema.as_ref().and_then(|s| s.open_api_v3_schema.as_ref()).is_some());
		}
	}
}
