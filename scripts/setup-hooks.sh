#!/usr/bin/env bash
set -e

echo "Configuring git hooks..."
git config core.hooksPath .githooks

echo "Checking for cargo..."
if ! command -v cargo &>/dev/null; then
  echo "ERROR: cargo not found on PATH."
  echo "Install Rust via https://rustup.rs or Homebrew (brew install rust), then re-run this script."
  exit 1
fi

echo "cargo found at: $(command -v cargo)"
echo "Setup complete. The pre-commit hook will run 'cargo fmt --all -- --check' on every commit."
