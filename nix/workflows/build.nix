# SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
#
# SPDX-License-Identifier: Apache-2.0

{ config, ... }:
let
  allowed-actions = config.famedly.standards.allowed-action-versions;

  # Not (yet) part of the engineering standards' allow-list
  # (`standards/allowed-github-actions.toml`), so we pin them here the
  # same way: by full commit SHA.
  #
  # rev = "v7.0.1"
  upload-artifact = "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a";
  # rev = "v8.0.1"
  download-artifact = "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c";
in
{
  perSystem.githubActions.workflows.build = {
    name = "Build";

    on.pullRequest = {
      branches = [ "**" ];
      types = [
        "opened"
        "reopened"
        "synchronize"
        "ready_for_review"
      ];
    };
    on.mergeGroup = { };
    on.push = {
      branches = [ "main" ];
      tags = [ "v*" ];
    };

    concurrency = {
      group = "\${{ github.workflow }}-\${{ github.ref }}";
      cancelInProgress = true;
    };

    permissions = {
      # `write` is required to create releases for tag builds.
      contents = "write";
    };

    # Build the static binary and the Docker image natively on each
    # architecture and hand both to the jobs below as artifacts. The
    # artifact names carry the architecture so that the per-arch
    # results do not collide.
    jobs.build = {
      strategy.matrix.include = [
        {
          arch = "x86_64";
          runner = "ubuntu-26.04";
        }
        {
          arch = "aarch64";
          runner = "ubuntu-26.04-arm";
        }
      ];

      runsOn = "\${{ matrix.runner }}";

      steps = [
        { uses = allowed-actions."actions/checkout".uses; }
        { uses = allowed-actions."cachix/install-nix-action".uses; }

        {
          name = "Build static binary";
          run = ''
            nix build .#cnoidc --print-build-logs -o result-cnoidc
          '';
        }

        {
          name = "Build Docker image";
          run = "nix build .#docker-image --print-build-logs";
        }

        {
          name = "Upload static binary as artifact";
          uses = upload-artifact;
          with_ = {
            name = "cnoidc-linux-\${{ matrix.arch }}";
            path = "result-cnoidc/bin/cnoidc";
            if-no-files-found = "error";
          };
        }

        {
          name = "Upload Docker image as artifact";
          uses = upload-artifact;
          with_ = {
            name = "docker-image-\${{ matrix.arch }}";
            path = "result";
            if-no-files-found = "error";
          };
        }
      ];
    };

    # The chart is architecture-independent; lint and package it once. For
    # everything but version tags, a pre-release build of the chart is
    # additionally pushed to the nightly chart registry (helm) so that PRs
    # can be tested end to end; version tags are published by the release
    # job below.
    jobs.chart = {
      runsOn = "ubuntu-26.04-arm";

      steps = [
        { uses = allowed-actions."actions/checkout".uses; }
        { uses = allowed-actions."cachix/install-nix-action".uses; }

        {
          name = "Check that the committed CRDs match the code";
          run = "nix run .#check-crds";
        }

        {
          name = "Lint and package Helm chart";
          run = "nix build .#chart --print-build-logs -o result-chart";
        }

        {
          name = "Upload Helm chart as artifact";
          uses = upload-artifact;
          with_ = {
            name = "helm-chart";
            path = "result-chart/*.tgz";
            if-no-files-found = "error";
          };
        }

        {
          name = "Push nightly Helm chart to registry";
          if_ = "!startsWith(github.ref, 'refs/tags/')";
          shell = "nix develop .#rust --command bash -e {0}";
          env = {
            REGISTRY_USER = "\${{ vars.REGISTRY_USER }}";
            REGISTRY_PASSWORD = "\${{ secrets.registry_password || secrets.GITHUB_TOKEN }}";
            TAG = "\${{ github.head_ref || github.ref_name || 'latest' }}";
          };
          run = ''
            version="$(helm show chart result-chart/*.tgz | sed -n 's/^version: //p')"
            # SemVer pre-release identifiers only allow [0-9A-Za-z-].
            branch="$(printf '%s' "$TAG" | sed 's/[^A-Za-z0-9-]/-/g')"
            nightly="$version-$branch.''${GITHUB_SHA::7}"

            # Point the chart at the image the docker job pushes for this
            # very commit, so that installing the chart needs no overrides.
            cp -r charts/cnoidc chart
            sed -i 's|registry.famedly.net/docker-oss/cnoidc|registry.famedly.net/docker-nightly/cnoidc|' \
              chart/values.yaml
            helm package chart \
              --version "$nightly" \
              --app-version "$GITHUB_SHA" \
              --destination nightly

            echo "$REGISTRY_PASSWORD" \
              | helm registry login registry.famedly.net -u "$REGISTRY_USER" --password-stdin
            helm push nightly/*.tgz oci://registry.famedly.net/helm

            {
              echo "Nightly chart pushed. Install with:"
              echo
              echo '```sh'
              echo "helm install cnoidc oci://registry.famedly.net/helm/cnoidc --version $nightly \\"
              echo "  --namespace cnoidc-system --create-namespace --set zapp.url=https://zapp.example.com"
              echo '```'
            } >> "$GITHUB_STEP_SUMMARY"
          '';
        }
      ];
    };

    # Push images for all builds: version tags go to the release
    # registry (docker-oss), everything else (PRs, main) goes to the
    # nightly registry (docker-nightly). The per-architecture images
    # are combined into a single multi-arch manifest.
    jobs.docker = {
      runsOn = "ubuntu-26.04-arm";
      if_ = "github.event_name == 'push' || github.event_name == 'pull_request'";
      needs = [ "build" ];

      steps = [
        {
          name = "Download Docker images";
          uses = download-artifact;
          with_ = {
            pattern = "docker-image-*";
            path = "artifacts";
          };
        }

        {
          name = "Push multi-arch Docker manifest to registry";
          env = {
            REGISTRY_USER = "\${{ vars.REGISTRY_USER }}";
            REGISTRY_PASSWORD = "\${{ secrets.registry_password || secrets.GITHUB_TOKEN }}";
            TAG = "\${{ github.head_ref || github.ref_name || 'latest' }}";
          };
          run = ''
            if [[ "$GITHUB_REF_NAME" =~ v[0-9]+\.[0-9]+\.[0-9]+ ]]; then
              registry=registry.famedly.net/docker-oss
            else
              registry=registry.famedly.net/docker-nightly
            fi

            echo "$REGISTRY_PASSWORD" \
              | podman login registry.famedly.net -u "$REGISTRY_USER" --password-stdin

            image="$registry/cnoidc"
            # Branch names may contain slashes, which are not valid in
            # Docker tags.
            tag="''${TAG//\//-}"

            # Combine the per-arch images into a multi-arch manifest.
            # Every `podman load` overwrites `cnoidc:latest`, so retag
            # each image with an arch suffix before loading the next.
            podman manifest create cnoidc-multiarch
            for arch in x86_64 aarch64; do
              podman load < "artifacts/docker-image-$arch/result"
              podman tag cnoidc:latest "cnoidc:$arch"
              podman manifest add cnoidc-multiarch "containers-storage:localhost/cnoidc:$arch"
            done

            # `--all` pushes the per-arch images along with the
            # manifest (by digest only, so no arch-specific tags show
            # up in the registry). Publish under both the branch/tag
            # name and the commit SHA.
            podman manifest push --all cnoidc-multiarch "docker://$image:$tag"
            podman manifest push --all cnoidc-multiarch "docker://$image:$GITHUB_SHA"
          '';
        }
      ];
    };

    # For version tags, publish a single GitHub release with the static
    # binaries of both architectures and the Helm chart attached, and push
    # the chart to the OCI registry so that it can be installed with
    # `helm install cnoidc oci://registry.famedly.net/helm-oss/cnoidc`.
    jobs.release = {
      runsOn = "ubuntu-latest";
      if_ = "startsWith(github.ref, 'refs/tags/')";
      needs = [
        "build"
        "chart"
      ];

      steps = [
        {
          name = "Download static binaries";
          uses = download-artifact;
          with_ = {
            pattern = "cnoidc-linux-*";
            path = "artifacts";
          };
        }

        {
          name = "Download Helm chart";
          uses = download-artifact;
          with_ = {
            name = "helm-chart";
            path = "chart";
          };
        }

        {
          # `gh` is preinstalled on GitHub runners.
          name = "Create GitHub release";
          env = {
            GH_TOKEN = "\${{ github.token }}";
            GH_REPO = "\${{ github.repository }}";
          };
          run = ''
            tag="''${GITHUB_REF#refs/tags/}"

            # The file inside each artifact is just called `cnoidc`;
            # rename it to the artifact's arch-suffixed name for the
            # release assets.
            for dir in artifacts/cnoidc-linux-*; do
              install -m755 "$dir/cnoidc" "$(basename "$dir")"
            done

            gh release create "$tag" \
              --verify-tag \
              cnoidc-linux-* chart/*.tgz
          '';
        }

        {
          # `helm` is preinstalled on GitHub runners.
          name = "Push Helm chart to registry";
          env = {
            REGISTRY_USER = "\${{ vars.REGISTRY_USER }}";
            REGISTRY_PASSWORD = "\${{ secrets.registry_password || secrets.GITHUB_TOKEN }}";
          };
          run = ''
            echo "$REGISTRY_PASSWORD" \
              | helm registry login registry.famedly.net -u "$REGISTRY_USER" --password-stdin
            helm push chart/*.tgz oci://registry.famedly.net/helm-oss
          '';
        }
      ];
    };
  };
}
