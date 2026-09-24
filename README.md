<!--
SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)

SPDX-License-Identifier: AGPL-3.0-or-later
-->

# cnoidc — Cloud-native OIDC

A Kubernetes operator that manages OIDC applications and roles in a
[Zitadel](https://zitadel.com/) project through custom resources.

Each cluster owns one Zitadel project, provisioned by a
[zapp](https://github.com/famedly/zapp) server on the strength of the cluster's
service account tokens. cnoidc presents its own token to zapp, receives a
credential for the project, and syncs `OIDCApplication` and `ProjectRole`
resources into it. What Zitadel assigns — above all the **client ID** — is
published in ConfigMaps for workloads to consume, together with the Zitadel URL
and project ID.

The design follows ExternalDNS: the chart installs the CRDs and one operator
that talks to a single upstream server (zapp), and everything else is declared
in the namespaces where the applications live.

```
OIDCApplication / ProjectRole        cnoidc                    zapp             Zitadel
  (CRDs in the cluster)                |                         |                 |
       |  create / update / delete     |                         |                 |
       |  (or every sync_interval) --> |                         |                 |
       |                               | POST /v1/credentials?type=key             |
       |                               |   Bearer <projected SA token> ---------> |                 |
       |                               |   { project_id, key, zitadel_url } <---- |                 |
       |                               | (key kept in memory only)                |                 |
       |                               | create / update app, role ---------------------------> |
       |                               |   { appId, clientId } <------------------------------- |
       |  ConfigMap <name>-oidc /      |                                                        |
       |  <name>-role; status  <------ |                                                        |
```

## How it works

### Credential

cnoidc holds no long-lived Zitadel secret. The chart mounts a
[projected service account token](https://kubernetes.io/docs/tasks/configure-pod-container/configure-service-account/#serviceaccount-token-volume-projection)
with the audience zapp expects (`zapp` by default). On start and whenever the
credential nears expiry, cnoidc reads that token and calls zapp's
`POST /v1/credentials?type=key`. zapp verifies it against the cluster's public
OpenID discovery documents, provisions (or adopts) the project for the cluster's
issuer, and returns a **machine key**.

The key is never persisted:

- It is written into a memory-backed `emptyDir` (`/var/run/cnoidc`) for the
  instant the Zitadel client library reads it — the library only accepts a file
  — and removed right after. From then on the key exists in process memory only,
  where it signs short-lived JWTs that are exchanged for access tokens.
- It is renewed after two thirds of its lifetime, so a zapp outage of up to a
  third of the lifetime goes unnoticed. A stale-but-valid key is kept as
  fallback while zapp is unreachable.
- If Zitadel rejects the credential (e.g. zapp's garbage collection deactivated
  the account while the cluster was down), the next reconcile requests a new one
  and retries.
- A restart simply asks zapp again; zapp adopts the existing project.

### Reconciliation

Every resource is reconciled when it changes and again every `sync_interval`
(default `10m`), so that drift in Zitadel — someone editing the application in
the console, a deleted ConfigMap — is repaired eventually. Reconciles are
idempotent and converge:

- **`OIDCApplication`** ensures an OIDC application named `<namespace>/<name>`
  exists in the project with the spec's configuration. Missing: created. Present
  (found by the ID remembered in `status`, else by name): reactivated if
  inactive, renamed if needed, updated on drift. Only OIDC applications are
  adopted; an API or SAML app of the same name is left alone.
- **`ProjectRole`** ensures a role with the spec's key (default: the resource
  name) exists with the given display name and group. Keys are global to the
  project, so they must be unique across the cluster; the key is immutable
  (changing it in the spec is rejected in `status`, delete and recreate
  instead).
- A **finalizer** removes the application or role from Zitadel before the
  resource disappears. The ConfigMaps and Secrets are owned by the resource and
  garbage-collected by Kubernetes.
- `status.conditions` carries a `Ready` condition with the last error when
  something failed; `kubectl get oidcapp` shows client ID and readiness.

### Outputs

For an `OIDCApplication` named `frontend`, ConfigMap `frontend-oidc` (name
configurable via `spec.configMapName`) contains:

| Key | Value | | --------------------- |
----------------------------------------------- | | `OIDC_CLIENT_ID` | Client ID
assigned by Zitadel | | `OIDC_APPLICATION_ID` | Application ID in Zitadel | |
`ZITADEL_URL` | Base URL of the Zitadel instance, with `/` | | `ZITADEL_ISSUER`
| The same without trailing slash (OIDC `issuer`) | | `ZITADEL_PROJECT_ID` | ID
of the cluster's project |

With `authMethod: basic` or `post`, Secret `frontend-oidc` (`spec.secretName`)
additionally holds `OIDC_CLIENT_ID` and `OIDC_CLIENT_SECRET`. Zitadel reveals
the secret only at creation; if the Secret goes missing, cnoidc regenerates the
client secret (the old one stops working).

For a `ProjectRole` named `admin`, ConfigMap `admin-role` contains:

| Key | Value | | --------------------- |
------------------------------------------------ | | `ZITADEL_ROLE_KEY` | Role
key as it appears in tokens | | `ZITADEL_ROLES_CLAIM` |
`urn:zitadel:iam:org:project:<project id>:roles` | | `ZITADEL_URL` | Base URL of
the Zitadel instance | | `ZITADEL_ISSUER` | The same without trailing slash | |
`ZITADEL_PROJECT_ID` | ID of the cluster's project |

Zitadel has no separate ID for roles; the key is the identifier. See
[`examples/resources.yaml`](examples/resources.yaml) for consuming these from a
workload.

## Custom resources

Both live in `cnoidc.famedly.com/v1alpha1` and are namespaced.

### `OIDCApplication` (`oidcapp`)

```yaml
apiVersion: cnoidc.famedly.com/v1alpha1
kind: OIDCApplication
metadata:
  name: frontend
spec:
  redirectUris: [https://app.example.com/callback]
  postLogoutRedirectUris: [https://app.example.com/]
  appType: web # web | userAgent | native
  authMethod: none # none | basic | post | privateKeyJwt
  grantTypes: [authorizationCode] # + implicit | refreshToken | deviceCode | tokenExchange
  responseTypes: [code] # + idToken | idTokenToken
  accessTokenType: bearer # bearer | jwt
  accessTokenRoleAssertion: false
  idTokenRoleAssertion: false
  idTokenUserinfoAssertion: false
  devMode: false # allow http:// redirect URIs
  additionalOrigins: []
  clockSkew: 5s # optional, at most 5s
  backChannelLogoutUri: "" # optional
  skipNativeAppSuccessPage: false
  configMapName: frontend-oidc # default <name>-oidc
  secretName: frontend-oidc # default <name>-oidc
```

Only `redirectUris` is normally needed; the defaults describe a public
authorization-code client with PKCE.

### `ProjectRole` (`zrole`)

```yaml
apiVersion: cnoidc.famedly.com/v1alpha1
kind: ProjectRole
metadata:
  name: admin
spec:
  key: shop.admin # default: resource name; immutable
  displayName: Shop administrator # default: key
  group: shop # optional
  configMapName: admin-role # default <name>-role
```

The full schemas are in
[`charts/cnoidc/crds/`](charts/cnoidc/crds/cnoidc.famedly.com.yaml), generated
from [`src/crd.rs`](src/crd.rs) by `cnoidc crd`.

## Installation

### Prerequisites

1. A running zapp with a `[[grants]]` entry matching this cluster's
   `--service-account-issuer` URL, and the cluster's
   `/.well-known/openid-configuration` and JWKS served publicly at that URL
   (zapp verifies tokens against them). See zapp's README.
1. zapp's `verification.audience` (default `zapp`) — pass the same as
   `zapp.audience` if it differs.

### Helm

```sh
helm install cnoidc oci://registry.famedly.net/helm-oss/cnoidc \
  --namespace cnoidc-system --create-namespace \
  --set zapp.url=https://zapp.example.com
```

or from a checkout: `helm install cnoidc charts/cnoidc --set zapp.url=...`.

Every PR and push to `main` also publishes a pre-release chart to the nightly
registry, versioned `<version>-<branch>.<short sha>` and pointing at the image
built for the same commit (the exact `helm install` line is in the workflow's
step summary):

```sh
helm install cnoidc oci://registry.famedly.net/helm/cnoidc \
  --version 0.1.0-my-branch.0123abc --set zapp.url=https://zapp.example.com
```

Notable values ([`values.yaml`](charts/cnoidc/values.yaml)):

| Value | Default | Meaning | | ----------------------- | ------------- |
------------------------------------------------------- | | `zapp.url` | — |
zapp base URL (required) | | `zapp.audience` | `zapp` | Audience of the
projected token | | `watchNamespace` | `""` (all) | Restrict to one namespace
(Role instead of ClusterRole) | | `syncInterval` | `10m` | Full re-sync cadence
| | `networkPolicy.enabled` | `false` | Egress to DNS/API server +
`networkPolicy.egress` | | `logLevel` | `cnoidc=info` | `RUST_LOG` filter |

Helm installs the CRDs from `crds/` on first install but never upgrades them;
after a chart upgrade, `kubectl apply -f charts/cnoidc/crds/` (or run with
`--skip-crds` and manage them separately).

The operator runs as a single replica with `Recreate` strategy: it holds no
leader lease, and two instances would race on the same Zitadel objects.

### Outside the cluster

```sh
kubectl create token cnoidc --audience=zapp > /tmp/token
cnoidc cnoidc.toml   # token_file = "/tmp/token"; uses the current kubeconfig
```

See [`cnoidc.example.toml`](cnoidc.example.toml) for all options.

## Development

```sh
nix develop                       # Rust toolchain, helm, cargo-nextest
cargo nextest run --all-targets
nix run .#generate-crds           # after editing src/crd.rs
nix build .#chart                 # lint + package the chart
nix build .#docker-image
```

CI (`.github/workflows/`) is generated from `nix/workflows/*.nix` through the
[engineering standards](https://github.com/famedly/engineering-standards);
regenerate with `nix run .#filegen-activate`. The `check-pre-commit-hooks`
workflow runs the pre-commit hooks, including a REUSE check and a check that the
committed CRDs match the code.

## Trust model

- The projected token is only good for zapp: its audience makes the API server
  reject it, and it is minted for one hour with in-place rotation.
- The machine key acts as the project's `PROJECT_OWNER` service account. That is
  enough to manage applications and roles of this cluster's project and nothing
  else; zapp's trust model covers what such an account can and cannot do.
- Client secrets are the only Zitadel secrets that reach persistent storage, as
  Kubernetes Secrets in the application's namespace, because workloads need
  them. Everything else in the ConfigMaps is public.
- The container runs as non-root with a read-only root filesystem; the only
  writable mount is the memory-backed key scratch directory.

## Future work

- **Events**: emit Kubernetes events alongside status conditions.
- **Metrics**: Prometheus metrics for reconcile counts and credential age.
- **API applications** and **project grants** to other organisations, once
  needed.
- **Cross-namespace role references** on applications, if Zitadel gains
  per-application role scoping.
