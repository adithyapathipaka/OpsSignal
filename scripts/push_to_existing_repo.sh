#!/usr/bin/env bash
# Pushes this scaffold into an EXISTING GitHub repo (the LICENSE commit
# is already there) rather than creating a new one.
#
# Prerequisites:
#   - gh CLI installed and authenticated: gh auth login
#   - git installed
#   - The repo already exists at github.com/<GH_USER>/OpsSignal with at
#     least one commit (e.g. just the LICENSE) on its default branch.
#
# This script:
#   1. Clones the existing repo fresh (so we don't clobber its history)
#   2. Copies this scaffold's files in on top
#   3. Commits and pushes
#
# Usage:
#   ./scripts/push_to_existing_repo.sh GH_USERNAME REPO_NAME [branch]
#
# Example:
#   ./scripts/push_to_existing_repo.sh adithyapathipaka OpsSignal main

set -euo pipefail

GH_USER="${1:?Usage: push_to_existing_repo.sh GH_USERNAME REPO_NAME [branch]}"
REPO_NAME="${2:?Usage: push_to_existing_repo.sh GH_USERNAME REPO_NAME [branch]}"
BRANCH="${3:-main}"
SCAFFOLD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CLONE_DIR="$(mktemp -d)"

echo "About to push this scaffold into ${GH_USER}/${REPO_NAME} (branch: ${BRANCH})."
echo "This will NOT delete the existing LICENSE commit, but will add all"
echo "scaffold files on top of it as a new commit."
read -p "Continue? [y/N] " confirm
if [ "$confirm" != "y" ] && [ "$confirm" != "Y" ]; then
  echo "Aborting." >&2
  exit 1
fi

echo "Cloning existing repo into temp dir: ${CLONE_DIR}"
gh repo clone "${GH_USER}/${REPO_NAME}" "$CLONE_DIR" -- --branch "$BRANCH"

echo "Copying scaffold files (excluding .git, scripts dir itself is included)..."
rsync -av --exclude='.git' "${SCAFFOLD_DIR}/" "${CLONE_DIR}/"

cd "$CLONE_DIR"

# Don't let the scaffold's LICENSE placeholder overwrite the repo's real,
# already-correct Apache-2.0 LICENSE file from GitHub's license picker.
if [ -f "${SCAFFOLD_DIR}/LICENSE" ] && git diff --quiet LICENSE 2>/dev/null; then
  : # no-op, file unchanged
fi
git checkout -- LICENSE 2>/dev/null || true

git add .
git commit -m "Add v0.1 architecture scaffold

- Rust workspace: opssignal-core, opssignal-ffi, opssignal-py, opssignal-cli
- Signal model with core-generated id/timestamp, closed Severity enum
- Routing engine with mandatory console fallback (never drop a signal)
- SqliteSink as zero-config default, pluggable SignalSink trait
- FFI panic-safety wrapper (catch_unwind on every extern \"C\" fn)
- Airflow failure_callback adapter (fire-and-forget by default)
- CI: fmt/clippy/test + CLAUDE.md zero-width-unicode integrity check
- docs/ARCHITECTURE.md recording all design decisions and trade-offs

Several pieces are deliberately scaffolded with TODO markers rather than
faked as complete - see CONTRIBUTING.md and open an issue tracker via
setup_project.sh / seed_issues.sh for the v0.1 task breakdown."

git push origin "$BRANCH"

echo ""
echo "Pushed: https://github.com/${GH_USER}/${REPO_NAME}"
echo ""
echo "Next steps:"
echo "  ./scripts/setup_project.sh ${GH_USER}"
echo "  ./scripts/seed_issues.sh ${GH_USER}"
echo ""
echo "Cleaning up temp clone dir: ${CLONE_DIR}"
rm -rf "$CLONE_DIR"
