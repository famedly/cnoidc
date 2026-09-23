# SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
#
# SPDX-License-Identifier: Apache-2.0

{ inputs, ... }: {
  perSystem =
    { config, pkgs, ... }:
    let
      mkCnoidc =
        rustPlatform:
        rustPlatform.buildRustPackage {
          pname = "cnoidc";
          version = (pkgs.lib.importTOML ../Cargo.toml).package.version;

          src = pkgs.lib.fileset.toSource {
            root = ../.;
            fileset = pkgs.lib.fileset.unions [
              ../Cargo.toml
              ../Cargo.lock
              ../build.rs
              ../src
            ];
          };

          cargoLock.lockFile = ../Cargo.lock;

          # ensure that the flake is always built from a clean tree, so
          # the binary contains the correct commit hash to fulfill AGPL requirements
          env.GIT_COMMIT_HASH = inputs.self.rev or "dirty";

          meta = {
            description = "Kubernetes operator syncing OIDC applications and roles to Zitadel";
            mainProgram = "cnoidc";
          };
        };
    in
    {
      packages = {
        cnoidc = mkCnoidc pkgs.pkgsStatic.rustPlatform;
        default = config.packages.cnoidc;

        docker-image = pkgs.dockerTools.buildLayeredImage {
          name = "cnoidc";
          tag = "latest";

          # TLS roots for talking to zapp and Zitadel.
          contents = [ pkgs.cacert ];

          config = {
            Entrypoint = [ (pkgs.lib.getExe config.packages.cnoidc) ];
            ExposedPorts."8080/tcp" = { };
            User = "65532:65532";
          };
        };
      };
    };
}
