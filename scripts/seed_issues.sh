#!/usr/bin/env bash
# Creates v0.1-scope issues in the repo, matching the architecture doc's
# MVP scope exactly. Run after create_repo.sh and setup_project.sh.
#
# Usage:
#   ./scripts/seed_issues.sh YOUR_GH_USERNAME

set -euo pipefail

GH_USER="${1:?Usage: seed_issues.sh YOUR_GH_USERNAME}"
REPO="${GH_USER}/opssignal"

create_issue() {
  local title="$1"
  local body="$2"
  local label="$3"
  echo "Creating: $title"
  gh issue create --repo "$REPO" --title "$title" --body "$body" --label "$label"
}

create_issue \
  "Wire SignalClient into PyO3 notify()" \
  "crates/python/src/lib.rs currently validates input but discards it (see TODO comment). Build a process-global, lazily-initialized SignalClient from signal.yaml / \$SIGNAL_CONFIG and call client.notify_async() from notify()." \
  "v0.1"

create_issue \
  "Wire SignalClient into signal-cli" \
  "crates/cli/src/main.rs is scaffolded but exits 1 with a TODO. Construct a SignalClient from the local config and call notify_sync with a default timeout so CI/shell usage gets a real exit code." \
  "v0.1"

create_issue \
  "Config loader: signal.yaml -> RoutingConfig + provider instances" \
  "Need a loader that reads signal.yaml (see python/examples/signal.example.yaml), interpolates \${ENV_VAR} references, and constructs the Provider trait objects + RoutingConfig + SqliteSink that SignalClient needs." \
  "v0.1"

create_issue \
  "Integration test: Airflow failure_callback end-to-end" \
  "Using a mocked Airflow context dict, verify failure_callback() produces a signal that routes correctly and lands in the SQLite sink. Use wiremock for the Slack webhook side." \
  "v0.1"

create_issue \
  "Publish first 0.1.0 release to PyPI and crates.io" \
  "Manual first publish (required before Trusted Publishing / OIDC can be configured for CI-based releases). Verify the opssignal name is still available on both registries immediately before publishing." \
  "v0.1"

create_issue \
  "Deduplication using the existing sink as a key store" \
  "v0.2 per roadmap. Use the SQLite sink dedup_key column (already indexed) rather than introducing Redis. Define a TTL/window for what counts as a duplicate." \
  "v0.2"

create_issue \
  "Teams + Alertmanager providers" \
  "v0.2 per roadmap. Implement the Provider trait for both; should require zero changes to routing.rs or lib.rs." \
  "v0.2"

create_issue \
  "Postgres sink implementation" \
  "v0.3 per roadmap. Implements SignalSink; opt-in via signal.yaml storage.sink: postgres. Becomes the recommended choice once users run multi-worker Airflow (SQLite single-writer contention)." \
  "v0.3"

echo ""
echo "Done. View issues: gh issue list --repo $REPO"
