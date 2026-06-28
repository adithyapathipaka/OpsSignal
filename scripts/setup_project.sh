#!/usr/bin/env bash
# Sets up a GitHub Project (v2) for opssignal with columns matching the
# v0.1-v0.5 roadmap, and seeds it with issues for the v0.1 MVP scope.
#
# Prerequisites:
#   - gh CLI installed and authenticated: gh auth login
#   - gh CLI project extension scopes: gh auth refresh -s project
#   - The repo must already exist (run create_repo.sh first)
#
# Usage:
#   ./scripts/setup_project.sh YOUR_GH_USERNAME

set -euo pipefail

GH_USER="${1:?Usage: setup_project.sh YOUR_GH_USERNAME}"
REPO="opssignal"

echo "Creating GitHub Project for ${GH_USER}/${REPO}..."

PROJECT_NUMBER=$(gh project create \
  --owner "$GH_USER" \
  --title "opssignal roadmap" \
  --format json | jq -r '.number')

echo "Created project #${PROJECT_NUMBER}"

# Add a Status field with values matching the roadmap stages.
# gh project field-create supports SINGLE_SELECT with options.
gh project field-create "$PROJECT_NUMBER" \
  --owner "$GH_USER" \
  --name "Milestone" \
  --data-type SINGLE_SELECT \
  --single-select-options "v0.1,v0.2,v0.3,v0.4,v0.5+,Backlog"

echo "Linking repository..."
gh project link "$PROJECT_NUMBER" --owner "$GH_USER" --repo "$REPO"

echo ""
echo "Project created: https://github.com/users/${GH_USER}/projects/${PROJECT_NUMBER}"
echo "Next: run ./scripts/seed_issues.sh ${GH_USER} to create v0.1 scope issues."
