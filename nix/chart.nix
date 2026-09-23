# SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
#
# SPDX-License-Identifier: Apache-2.0

# The Helm chart. Its CRDs are generated from the Rust types in
# `src/crd.rs` (`cnoidc crd`) and committed to `charts/cnoidc/crds/`, so
# that the chart is usable straight from the repository; `check-crds`
# guards against the two drifting apart.
{ ... }: {
  perSystem =
    { config, pkgs, ... }:
    let
      chartDir = ../charts/cnoidc;
      chartVersion = (pkgs.lib.importTOML ../Cargo.toml).package.version;
      cnoidc = pkgs.lib.getExe config.packages.cnoidc;
    in
    {
      packages = {
        # Packaged chart (`cnoidc-<version>.tgz`), with the app and chart
        # versions taken from Cargo.toml.
        chart =
          pkgs.runCommand "cnoidc-chart-${chartVersion}" { nativeBuildInputs = [ pkgs.kubernetes-helm ]; }
            ''
              cp -r ${chartDir} chart
              chmod -R u+w chart
              mkdir -p $out
              helm lint chart
              helm package chart \
                --version ${chartVersion} \
                --app-version ${chartVersion} \
                --destination $out
            '';
      };

      apps = {
        # Regenerate `charts/cnoidc/crds/` from the binary.
        generate-crds = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "generate-crds";
              text = ''
                ${cnoidc} crd > charts/cnoidc/crds/cnoidc.famedly.com.yaml
              '';
            }
          );
        };

        # Fail if the committed CRDs do not match the binary's.
        check-crds = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "check-crds";
              text = ''
                if ! diff -u charts/cnoidc/crds/cnoidc.famedly.com.yaml <(${cnoidc} crd); then
                  echo "charts/cnoidc/crds/ is out of date; run 'nix run .#generate-crds'" >&2
                  exit 1
                fi
              '';
            }
          );
        };
      };
    };
}
