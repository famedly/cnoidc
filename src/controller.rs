// SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reconciliation of `OIDCApplication` and `ProjectRole` resources.
//!
//! Every resource is reconciled when it changes and again every
//! `sync_interval`, so that drift in Zitadel is repaired eventually. A
//! finalizer ensures the Zitadel object is removed before the resource
//! disappears from the cluster. Reconciles never hold the Zitadel
//! credential themselves: they ask the [`SessionManager`] for the current
//! session and hand it back if Zitadel rejects it.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use anyhow::Context as _;
use futures::StreamExt as _;
use k8s_openapi::{
	api::core::v1::{ConfigMap, Secret},
	apimachinery::pkg::apis::meta::v1::{OwnerReference, Time},
};
use kube::{
	Api, Client, Resource, ResourceExt as _,
	api::{Patch, PatchParams},
	runtime::{
		Controller,
		controller::Action,
		finalizer::{Event, finalizer},
		reflector::ObjectRef,
		watcher,
	},
};
use serde::Serialize;
use tracing::{error, info, warn};

use crate::{
	crd::{
		CONDITION_READY, Condition, FINALIZER, OIDCApplication, OIDCApplicationStatus, ProjectRole,
		ProjectRoleStatus,
	},
	zapp::{Session, SessionManager, is_auth_error},
	zitadel::OidcSettings,
};

/// Field manager the operator uses for server-side apply.
const FIELD_MANAGER: &str = "cnoidc";

/// Label put on every ConfigMap and Secret the operator writes.
const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

/// Claim in Zitadel tokens carrying the project's roles, for consumers of
/// role ConfigMaps.
fn roles_claim(project_id: &str) -> String {
	format!("urn:zitadel:iam:org:project:{project_id}:roles")
}

pub struct Context {
	pub client: Client,
	pub sessions: Arc<SessionManager>,
	pub sync_interval: Duration,
	/// Namespace to watch; all namespaces when `None`.
	pub namespace: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("{0:#}")]
	Reconcile(#[from] anyhow::Error),
	#[error("finalizer: {0}")]
	Finalizer(#[source] Box<kube::runtime::finalizer::Error<Error>>),
}

impl From<kube::runtime::finalizer::Error<Error>> for Error {
	fn from(value: kube::runtime::finalizer::Error<Error>) -> Self {
		Error::Finalizer(Box::new(value))
	}
}

/// Runs both controllers until the process ends.
pub async fn run(ctx: Arc<Context>) {
	let (apps, roles, configmaps, secrets) = match &ctx.namespace {
		Some(ns) => (
			Api::<OIDCApplication>::namespaced(ctx.client.clone(), ns),
			Api::<ProjectRole>::namespaced(ctx.client.clone(), ns),
			Api::<ConfigMap>::namespaced(ctx.client.clone(), ns),
			Api::<Secret>::namespaced(ctx.client.clone(), ns),
		),
		None => (
			Api::<OIDCApplication>::all(ctx.client.clone()),
			Api::<ProjectRole>::all(ctx.client.clone()),
			Api::<ConfigMap>::all(ctx.client.clone()),
			Api::<Secret>::all(ctx.client.clone()),
		),
	};

	// Owned ConfigMaps/Secrets are watched too, so that deleting or editing
	// one triggers a repair of the owning resource.
	let owned = watcher::Config::default().labels(&format!("{MANAGED_BY_LABEL}={FIELD_MANAGER}"));

	let app_controller = Controller::new(apps, watcher::Config::default())
		.owns(configmaps.clone(), owned.clone())
		.owns(secrets, owned.clone())
		.shutdown_on_signal()
		.run(reconcile_application, error_policy, ctx.clone())
		.for_each(log_result::<OIDCApplication>);

	let role_controller = Controller::new(roles, watcher::Config::default())
		.owns(configmaps, owned)
		.shutdown_on_signal()
		.run(reconcile_role, error_policy, ctx.clone())
		.for_each(log_result::<ProjectRole>);

	futures::join!(app_controller, role_controller);
}

async fn log_result<K: Resource>(
	result: Result<(ObjectRef<K>, Action), kube::runtime::controller::Error<Error, watcher::Error>>,
) where
	K::DynamicType: std::fmt::Debug + std::hash::Hash + Eq + Clone + Unpin,
{
	match result {
		Ok((obj, _)) => tracing::debug!("reconciled {obj}"),
		Err(kube::runtime::controller::Error::ReconcilerFailed(e, obj)) => {
			warn!("reconciling {obj} failed: {e}");
		}
		Err(e) => error!("controller error: {e}"),
	}
}

fn error_policy<K>(_obj: Arc<K>, _err: &Error, ctx: Arc<Context>) -> Action {
	// Back off, but never longer than a regular sync would take anyway.
	Action::requeue(Duration::from_secs(30).min(ctx.sync_interval))
}

/// Runs `f` with the current Zitadel session. If Zitadel rejects the
/// credential, the session is invalidated and `f` retried once with a fresh
/// one.
async fn with_session<T, F, Fut>(sessions: &SessionManager, f: F) -> anyhow::Result<T>
where
	F: Fn(Arc<Session>) -> Fut,
	Fut: Future<Output = anyhow::Result<T>>,
{
	let session = sessions.session().await.context("obtaining Zitadel credential")?;
	match f(session.clone()).await {
		Err(e) if is_auth_error(&e) => {
			sessions.invalidate(&session).await;
			let session = sessions.session().await.context("obtaining Zitadel credential")?;
			f(session).await
		}
		other => other,
	}
}

// --- OIDCApplication

async fn reconcile_application(
	app: Arc<OIDCApplication>,
	ctx: Arc<Context>,
) -> Result<Action, Error> {
	let namespace = app.namespace().context("OIDCApplication without namespace")?;
	let api = Api::<OIDCApplication>::namespaced(ctx.client.clone(), &namespace);
	finalizer(&api, FINALIZER, app, |event| async {
		match event {
			Event::Apply(app) => apply_application(app, &ctx).await,
			Event::Cleanup(app) => cleanup_application(app, &ctx).await,
		}
	})
	.await
	.map_err(Error::from)
}

/// Zitadel-side name of an application: `<namespace>/<name>`, unique across
/// the cluster and readable in the console.
fn application_name(app: &OIDCApplication) -> String {
	format!("{}/{}", app.namespace().unwrap_or_default(), app.name_any())
}

async fn apply_application(app: Arc<OIDCApplication>, ctx: &Context) -> Result<Action, Error> {
	let namespace = app.namespace().context("OIDCApplication without namespace")?;
	let name = app.name_any();
	let api = Api::<OIDCApplication>::namespaced(ctx.client.clone(), &namespace);
	let zitadel_name = application_name(&app);
	let config_map_name =
		app.spec.config_map_name.clone().unwrap_or_else(|| format!("{name}-oidc"));
	let secret_name = app.spec.secret_name.clone().unwrap_or_else(|| format!("{name}-oidc"));
	let known_id = app.status.as_ref().and_then(|s| s.application_id.clone());

	let result: anyhow::Result<()> = async {
		let settings = OidcSettings::from_spec(&app.spec).context("invalid spec")?;
		let (session, synced) = with_session(&ctx.sessions, |session| {
			let settings = settings.clone();
			let zitadel_name = zitadel_name.clone();
			let known_id = known_id.clone();
			async move {
				let synced = session
					.zitadel
					.sync_application(&zitadel_name, known_id.as_deref(), &settings)
					.await?;
				Ok((session, synced))
			}
		})
		.await?;

		let owner = owner_reference(&*app);
		let data = BTreeMap::from([
			("ZITADEL_URL".to_string(), session.zitadel_url.to_string()),
			("ZITADEL_ISSUER".to_string(), session.issuer()),
			("ZITADEL_PROJECT_ID".to_string(), session.project_id.clone()),
			("OIDC_CLIENT_ID".to_string(), synced.client_id.clone()),
			("OIDC_APPLICATION_ID".to_string(), synced.app_id.clone()),
		]);
		apply_config_map(&ctx.client, &namespace, &config_map_name, owner.clone(), data).await?;

		let uses_secret = app.spec.auth_method.uses_client_secret();
		let secrets = Api::<Secret>::namespaced(ctx.client.clone(), &namespace);
		if uses_secret {
			// Zitadel only ever reveals the secret once. If the Secret went
			// missing, belongs to another application, or does not carry
			// the client ID the current secret was issued for (the method
			// just switched, or a previous write failed), a new one is
			// generated; otherwise the stored one is kept.
			let client_secret = match synced.client_secret {
				Some(secret) => Some(secret),
				None => match secrets.get_opt(&secret_name).await? {
					Some(existing)
						if is_owned_by(&existing.metadata, &*app)
							&& secret_has(&existing, "OIDC_CLIENT_SECRET")
							&& secret_is(&existing, "OIDC_CLIENT_ID", &synced.client_id) =>
					{
						None
					}
					_ => Some(session.zitadel.regenerate_client_secret(&synced.app_id).await?),
				},
			};
			if let Some(client_secret) = client_secret {
				let data = BTreeMap::from([
					("OIDC_CLIENT_ID".to_string(), synced.client_id.clone()),
					("OIDC_CLIENT_SECRET".to_string(), client_secret),
				]);
				apply_secret(&ctx.client, &namespace, &secret_name, owner, data).await?;
			}
		} else if let Some(existing) = secrets.get_opt(&secret_name).await?
			&& is_owned_by(&existing.metadata, &*app)
		{
			secrets.delete(&secret_name, &Default::default()).await?;
			info!("removed secret {namespace}/{secret_name}: application uses no client secret");
		}

		let status = OIDCApplicationStatus {
			observed_generation: app.metadata.generation,
			application_id: Some(synced.app_id),
			client_id: Some(synced.client_id),
			project_id: Some(session.project_id.clone()),
			config_map: Some(config_map_name.clone()),
			secret: uses_secret.then(|| secret_name.clone()),
			conditions: vec![ready_condition(
				app.metadata.generation,
				true,
				if synced.changed { "Updated" } else { "Synced" },
				&format!("application {zitadel_name} is in sync with Zitadel"),
			)],
		};
		patch_status(&api, &name, &status).await
	}
	.await;

	match result {
		Ok(()) => Ok(Action::requeue(ctx.sync_interval)),
		Err(e) => {
			let status = OIDCApplicationStatus {
				observed_generation: app.metadata.generation,
				conditions: vec![ready_condition(
					app.metadata.generation,
					false,
					"SyncFailed",
					&format!("{e:#}"),
				)],
				..app.status.clone().unwrap_or_default()
			};
			if let Err(status_err) = patch_status(&api, &name, &status).await {
				warn!("updating status of {namespace}/{name} failed: {status_err:#}");
			}
			Err(Error::Reconcile(e))
		}
	}
}

async fn cleanup_application(app: Arc<OIDCApplication>, ctx: &Context) -> Result<Action, Error> {
	let zitadel_name = application_name(&app);
	let known_id = app.status.as_ref().and_then(|s| s.application_id.clone());
	with_session(&ctx.sessions, |session| {
		let zitadel_name = zitadel_name.clone();
		let known_id = known_id.clone();
		async move {
			let app_id = match known_id {
				Some(id) => Some(id),
				None => session
					.zitadel
					.find_application(&zitadel_name)
					.await?
					.and_then(|app| app.id().cloned()),
			};
			if let Some(app_id) = app_id {
				session.zitadel.remove_application(&app_id).await?;
			}
			Ok(())
		}
	})
	.await?;
	info!("cleaned up application {zitadel_name}");
	// Owned ConfigMap and Secret are garbage-collected by Kubernetes.
	Ok(Action::await_change())
}

// --- ProjectRole

async fn reconcile_role(role: Arc<ProjectRole>, ctx: Arc<Context>) -> Result<Action, Error> {
	let namespace = role.namespace().context("ProjectRole without namespace")?;
	let api = Api::<ProjectRole>::namespaced(ctx.client.clone(), &namespace);
	finalizer(&api, FINALIZER, role, |event| async {
		match event {
			Event::Apply(role) => apply_role(role, &ctx).await,
			Event::Cleanup(role) => cleanup_role(role, &ctx).await,
		}
	})
	.await
	.map_err(Error::from)
}

/// Zitadel role key of a resource: the spec's key, or the resource name.
fn role_key(role: &ProjectRole) -> String {
	role.spec.key.clone().unwrap_or_else(|| role.name_any())
}

async fn apply_role(role: Arc<ProjectRole>, ctx: &Context) -> Result<Action, Error> {
	let namespace = role.namespace().context("ProjectRole without namespace")?;
	let name = role.name_any();
	let api = Api::<ProjectRole>::namespaced(ctx.client.clone(), &namespace);
	let key = role_key(&role);
	let display_name = role.spec.display_name.clone().unwrap_or_else(|| key.clone());
	let config_map_name =
		role.spec.config_map_name.clone().unwrap_or_else(|| format!("{name}-role"));

	let result: anyhow::Result<()> = async {
		anyhow::ensure!(!key.is_empty(), "role key must not be empty");
		if let Some(previous) = role.status.as_ref().and_then(|s| s.key.as_deref())
			&& previous != key
		{
			anyhow::bail!(
				"role key is immutable: currently {previous:?}, spec says {key:?}; delete and recreate the resource to change it"
			);
		}
		let (session, changed) = with_session(&ctx.sessions, |session| {
			let key = key.clone();
			let display_name = display_name.clone();
			let group = role.spec.group.clone();
			async move {
				let changed =
					session.zitadel.sync_role(&key, &display_name, group.as_deref()).await?;
				Ok((session, changed))
			}
		})
		.await?;

		let data = BTreeMap::from([
			("ZITADEL_URL".to_string(), session.zitadel_url.to_string()),
			("ZITADEL_ISSUER".to_string(), session.issuer()),
			("ZITADEL_PROJECT_ID".to_string(), session.project_id.clone()),
			("ZITADEL_ROLE_KEY".to_string(), key.clone()),
			("ZITADEL_ROLES_CLAIM".to_string(), roles_claim(&session.project_id)),
		]);
		apply_config_map(&ctx.client, &namespace, &config_map_name, owner_reference(&*role), data)
			.await?;

		let status = ProjectRoleStatus {
			observed_generation: role.metadata.generation,
			key: Some(key.clone()),
			project_id: Some(session.project_id.clone()),
			config_map: Some(config_map_name.clone()),
			conditions: vec![ready_condition(
				role.metadata.generation,
				true,
				if changed { "Updated" } else { "Synced" },
				&format!("role {key} is in sync with Zitadel"),
			)],
		};
		patch_status(&api, &name, &status).await
	}
	.await;

	match result {
		Ok(()) => Ok(Action::requeue(ctx.sync_interval)),
		Err(e) => {
			let status = ProjectRoleStatus {
				observed_generation: role.metadata.generation,
				conditions: vec![ready_condition(
					role.metadata.generation,
					false,
					"SyncFailed",
					&format!("{e:#}"),
				)],
				..role.status.clone().unwrap_or_default()
			};
			if let Err(status_err) = patch_status(&api, &name, &status).await {
				warn!("updating status of {namespace}/{name} failed: {status_err:#}");
			}
			Err(Error::Reconcile(e))
		}
	}
}

async fn cleanup_role(role: Arc<ProjectRole>, ctx: &Context) -> Result<Action, Error> {
	// Remove the key the role was created with, not what the spec says now.
	let key = role.status.as_ref().and_then(|s| s.key.clone()).unwrap_or_else(|| role_key(&role));
	with_session(&ctx.sessions, |session| {
		let key = key.clone();
		async move { session.zitadel.remove_role(&key).await }
	})
	.await?;
	info!("cleaned up role {key}");
	Ok(Action::await_change())
}

// --- Kubernetes helpers

fn owner_reference<K>(owner: &K) -> OwnerReference
where
	K: Resource<DynamicType = ()>,
{
	let mut reference = owner.controller_owner_ref(&()).unwrap_or_default();
	reference.block_owner_deletion = Some(true);
	reference
}

fn is_owned_by<K>(metadata: &kube::api::ObjectMeta, owner: &K) -> bool
where
	K: Resource<DynamicType = ()>,
{
	let uid = owner.meta().uid.as_deref();
	metadata
		.owner_references
		.iter()
		.flatten()
		.any(|reference| uid.is_some() && Some(reference.uid.as_str()) == uid)
}

fn managed_labels() -> BTreeMap<String, String> {
	BTreeMap::from([(MANAGED_BY_LABEL.to_string(), FIELD_MANAGER.to_string())])
}

async fn apply_config_map(
	client: &Client,
	namespace: &str,
	name: &str,
	owner: OwnerReference,
	data: BTreeMap<String, String>,
) -> anyhow::Result<()> {
	let api = Api::<ConfigMap>::namespaced(client.clone(), namespace);
	let config_map = ConfigMap {
		metadata: kube::api::ObjectMeta {
			name: Some(name.to_string()),
			namespace: Some(namespace.to_string()),
			labels: Some(managed_labels()),
			owner_references: Some(vec![owner]),
			..Default::default()
		},
		data: Some(data),
		..Default::default()
	};
	api.patch(name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(&config_map))
		.await
		.with_context(|| format!("applying ConfigMap {namespace}/{name}"))?;
	Ok(())
}

async fn apply_secret(
	client: &Client,
	namespace: &str,
	name: &str,
	owner: OwnerReference,
	data: BTreeMap<String, String>,
) -> anyhow::Result<()> {
	let api = Api::<Secret>::namespaced(client.clone(), namespace);
	let secret = Secret {
		metadata: kube::api::ObjectMeta {
			name: Some(name.to_string()),
			namespace: Some(namespace.to_string()),
			labels: Some(managed_labels()),
			owner_references: Some(vec![owner]),
			..Default::default()
		},
		type_: Some("Opaque".to_string()),
		string_data: Some(data),
		..Default::default()
	};
	api.patch(name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(&secret))
		.await
		.with_context(|| format!("applying Secret {namespace}/{name}"))?;
	Ok(())
}

fn secret_has(secret: &Secret, key: &str) -> bool {
	secret.data.as_ref().is_some_and(|data| data.get(key).is_some_and(|v| !v.0.is_empty()))
}

fn secret_is(secret: &Secret, key: &str, value: &str) -> bool {
	secret.data.as_ref().is_some_and(|data| data.get(key).is_some_and(|v| v.0 == value.as_bytes()))
}

async fn patch_status<K, S>(api: &Api<K>, name: &str, status: &S) -> anyhow::Result<()>
where
	K: Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned,
	K::DynamicType: Default,
	S: Serialize,
{
	let patch = serde_json::json!({
		"apiVersion": K::api_version(&K::DynamicType::default()),
		"kind": K::kind(&K::DynamicType::default()),
		"status": status,
	});
	api.patch_status(name, &PatchParams::apply(FIELD_MANAGER).force(), &Patch::Apply(patch))
		.await
		.context("patching status")?;
	Ok(())
}

fn ready_condition(generation: Option<i64>, ready: bool, reason: &str, message: &str) -> Condition {
	Condition {
		type_: CONDITION_READY.to_string(),
		status: if ready { "True" } else { "False" }.to_string(),
		reason: reason.to_string(),
		// Kubernetes caps condition messages at 32 KiB; error chains are far
		// shorter, but stay well clear.
		message: message.chars().take(4096).collect(),
		observed_generation: generation,
		last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn names_follow_the_resource() {
		let app: OIDCApplication = serde_json::from_value(serde_json::json!({
			"apiVersion": "cnoidc.famedly.com/v1alpha1",
			"kind": "OIDCApplication",
			"metadata": { "name": "frontend", "namespace": "shop" },
			"spec": { "redirectUris": ["https://shop.example.com/cb"] },
		}))
		.expect("valid resource");
		assert_eq!(application_name(&app), "shop/frontend");

		let role: ProjectRole = serde_json::from_value(serde_json::json!({
			"apiVersion": "cnoidc.famedly.com/v1alpha1",
			"kind": "ProjectRole",
			"metadata": { "name": "admin", "namespace": "shop" },
			"spec": {},
		}))
		.expect("valid resource");
		assert_eq!(role_key(&role), "admin");
		let role: ProjectRole = serde_json::from_value(serde_json::json!({
			"apiVersion": "cnoidc.famedly.com/v1alpha1",
			"kind": "ProjectRole",
			"metadata": { "name": "admin", "namespace": "shop" },
			"spec": { "key": "shop.admin" },
		}))
		.expect("valid resource");
		assert_eq!(role_key(&role), "shop.admin");
	}

	#[test]
	fn roles_claim_names_the_project() {
		assert_eq!(roles_claim("123"), "urn:zitadel:iam:org:project:123:roles");
	}

	#[test]
	fn conditions_carry_generation() {
		let condition = ready_condition(Some(3), false, "SyncFailed", "boom");
		assert_eq!(condition.type_, "Ready");
		assert_eq!(condition.status, "False");
		assert_eq!(condition.observed_generation, Some(3));
	}
}
