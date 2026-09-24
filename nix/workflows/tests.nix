# SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)
#
# SPDX-License-Identifier: Apache-2.0

{ config, ... }:
let
  allowed-actions = config.famedly.standards.allowed-action-versions;
in
{
  perSystem.githubActions.workflows.tests = {
    name = "Run tests";

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

    concurrency = {
      group = "\${{ github.workflow }}-\${{ github.ref }}";
      cancelInProgress = true;
    };

    jobs.nextest = {
      runsOn = "ubuntu-26.04-arm";

      steps = [
        { uses = allowed-actions."actions/checkout".uses; }
        { uses = allowed-actions."cachix/install-nix-action".uses; }

        {
          name = "Run tests";
          shell = "nix develop .#rust --command bash {0}";
          run = "cargo nextest run --all-targets --all-features";
        }
      ];
    };

    # Render the chart with its default values and with every documented
    # option toggled, so that template errors surface before a release.
    jobs.helm = {
      runsOn = "ubuntu-26.04-arm";

      steps = [
        { uses = allowed-actions."actions/checkout".uses; }
        { uses = allowed-actions."cachix/install-nix-action".uses; }

        {
          name = "Lint and render Helm chart";
          shell = "nix develop .#rust --command bash -e {0}";
          run = ''
            helm lint charts/cnoidc
            helm template cnoidc charts/cnoidc --set zapp.url=https://zapp.example.com > /dev/null
            helm template cnoidc charts/cnoidc \
              --set zapp.url=https://zapp.example.com \
              --set watchNamespace=apps \
              --set serviceMonitor.enabled=true \
              --set podDisruptionBudget.enabled=true \
              --set networkPolicy.enabled=true > /dev/null
          '';
        }
      ];
    };
  };
}
