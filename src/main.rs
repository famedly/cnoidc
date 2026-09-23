// SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
//
// SPDX-License-Identifier: AGPL-3.0-or-later

mod config;
mod controller;
mod crd;
mod zapp;
mod zitadel;

use std::sync::Arc;

use anyhow::Context as _;
use axum::{Router, extract::State, http::StatusCode, routing::get};
use tracing::{info, warn};

use crate::{
	config::Config,
	zapp::{SessionManager, ZappClient},
};

/// Where this program's source lives. The commit hash makes the pointer
/// exact, satisfying the AGPL source offer: the build (`build.rs` locally,
/// the flake in releases) pins it to the tree the binary was built from.
const SOURCE_OFFER: &str = concat!(
	"source available at ",
	env!("CARGO_PKG_REPOSITORY"),
	"/commit/",
	env!("GIT_COMMIT_HASH"),
);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| "cnoidc=info".into()),
		)
		.init();

	let mut args = std::env::args().skip(1);
	match args.next().as_deref() {
		// `cnoidc crd` prints the CRDs; the Helm chart's `crds/` directory
		// is generated from it.
		Some("crd") => {
			print!("{}", crd::definitions_yaml()?);
			return Ok(());
		}
		Some(path) => run(Config::load(path)?).await,
		None => run(Config::load("cnoidc.toml")?).await,
	}
}

async fn run(config: Config) -> anyhow::Result<()> {
	info!("cnoidc {} ({})", env!("CARGO_PKG_VERSION"), SOURCE_OFFER);

	let client = kube::Client::try_default().await.context("connecting to Kubernetes")?;
	let sessions = Arc::new(SessionManager::new(ZappClient::new(config.zapp)?));

	// Fail loudly, but not fatally, if zapp is not reachable at startup:
	// resources are simply reconciled once it is.
	match sessions.session().await {
		Ok(session) => info!(
			"managing Zitadel project {} at {} as {}",
			session.project_id, session.zitadel_url, session.username
		),
		Err(e) => warn!("no Zitadel credential yet, will retry on reconcile: {e:#}"),
	}

	match &config.namespace {
		Some(ns) => info!("watching namespace {ns}"),
		None => info!("watching all namespaces"),
	}
	info!("full sync every {}", humantime::format_duration(config.sync_interval));

	let ctx = Arc::new(controller::Context {
		client,
		sessions: sessions.clone(),
		sync_interval: config.sync_interval,
		namespace: config.namespace,
	});

	let listener = tokio::net::TcpListener::bind(&config.listen)
		.await
		.with_context(|| format!("binding to {}", config.listen))?;
	info!("health endpoint listening on {}", config.listen);
	let server = tokio::spawn(
		axum::serve(listener, health_router(sessions))
			.with_graceful_shutdown(shutdown_signal())
			.into_future(),
	);

	// The controllers stop on SIGTERM/SIGINT (`shutdown_on_signal`); the
	// health endpoint does the same.
	controller::run(ctx).await;
	server.await.context("health endpoint")?.context("health endpoint")?;
	Ok(())
}

/// Resolves on SIGINT or SIGTERM, matching kube-runtime's
/// `shutdown_on_signal`.
async fn shutdown_signal() {
	use tokio::signal::unix::{SignalKind, signal};
	let mut sigterm = match signal(SignalKind::terminate()) {
		Ok(sigterm) => sigterm,
		Err(e) => {
			warn!("cannot listen for SIGTERM: {e}");
			let _ = tokio::signal::ctrl_c().await;
			return;
		}
	};
	tokio::select! {
		_ = tokio::signal::ctrl_c() => {}
		_ = sigterm.recv() => {}
	}
}

fn health_router(sessions: Arc<SessionManager>) -> Router {
	Router::new()
		.route("/", get(|| async { SOURCE_OFFER }))
		.route("/healthz", get(|| async { "ok" }))
		.route("/readyz", get(readyz))
		.with_state(sessions)
}

/// Ready once a Zitadel credential is at hand.
async fn readyz(State(sessions): State<Arc<SessionManager>>) -> (StatusCode, &'static str) {
	if sessions.is_ready().await {
		(StatusCode::OK, "ok")
	} else {
		(StatusCode::SERVICE_UNAVAILABLE, "no Zitadel credential")
	}
}
