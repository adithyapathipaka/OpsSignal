#!/usr/bin/env bash
set -e

echo "Configuring git hooks..."
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit

# ── Required: Rust ────────────────────────────────────────────────────────────
echo ""
echo "Checking for cargo (required)..."
if ! command -v cargo &>/dev/null; then
  echo "ERROR: cargo not found on PATH."
  echo "Install Rust: https://rustup.rs  or  brew install rust"
  exit 1
fi
echo "  cargo: $(command -v cargo)  ($(cargo --version))"

# ── Optional: ast-grep ────────────────────────────────────────────────────────
echo ""
echo "Checking for sg / ast-grep (optional)..."
if command -v sg &>/dev/null; then
  echo "  sg: $(command -v sg)  ($(sg --version 2>/dev/null || echo 'version unknown'))"
else
  echo "  sg not found — structural lint will be skipped locally."
  echo "  Install: npm install -g @ast-grep/cli"
fi

# ── Optional: ruff ────────────────────────────────────────────────────────────
echo ""
echo "Checking for ruff (optional)..."
if command -v ruff &>/dev/null; then
  echo "  ruff: $(command -v ruff)  ($(ruff --version))"
else
  echo "  ruff not found — Python lint will be skipped locally."
  echo "  Install: pip install ruff"
fi

echo ""
echo "Setup complete."
echo "Pre-commit runs: cargo fmt --check, cargo clippy, ast-grep (if sg installed), ruff (if installed)."
echo "Activate the Python venv with: source python/.venv/bin/activate"
echo "Run 'cargo test --workspace && cd python && pytest' manually before pushing."
