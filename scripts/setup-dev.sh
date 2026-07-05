#!/usr/bin/env bash
# setup-dev.sh — one-time dev environment bootstrap for new contributors.
#
# Run from the repo root:
#   bash scripts/setup-dev.sh
#
# What it does:
#   1. Verifies Rust >= 1.75 with rustfmt + clippy components
#   2. Installs uv (if missing) and creates .venv (repo root) via uv
#   3. Installs Python dev tools via uv sync (maturin, pytest, ruff, bandit)
#   4. Builds the Rust workspace and Python extension (maturin develop)
#   5. Runs the full test suite to confirm everything works
#   6. Installs git hooks (cargo fmt --check, clippy, ast-grep, ruff)
#   7. Installs ast-grep (optional, via npm)
#
# Safe to re-run — all steps are idempotent.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

BOLD='\033[1m'
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

step() { echo -e "\n${CYAN}${BOLD}==>${NC}${BOLD} $*${NC}"; }
ok()   { echo -e "  ${GREEN}ok${NC}  $*"; }
warn() { echo -e "  ${YELLOW}warn${NC}  $*"; }
die()  { echo -e "\n  ${RED}error${NC}  $*\n" >&2; exit 1; }

# ── 1. Rust ────────────────────────────────────────────────────────────────────
step "Checking Rust toolchain"

if ! command -v rustup &>/dev/null; then
  echo "  rustup not found. Installing..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
  source "$HOME/.cargo/env"
fi

if ! command -v cargo &>/dev/null; then
  [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
  command -v cargo &>/dev/null || die "cargo not found — open a new shell and retry."
fi

RUST_VERSION=$(rustc --version | awk '{print $2}')
RUST_MINOR=$(echo "$RUST_VERSION" | cut -d. -f2)
REQUIRED_MINOR=75  # matches rust-version in Cargo.toml

if [ "$RUST_MINOR" -lt "$REQUIRED_MINOR" ] 2>/dev/null; then
  die "Rust $RUST_VERSION found but >= 1.${REQUIRED_MINOR} required. Run: rustup update stable"
fi
ok "rustc $RUST_VERSION"

rustup component add rustfmt clippy --quiet
ok "rustfmt + clippy components present"

# ── 2. uv ─────────────────────────────────────────────────────────────────────
step "Checking uv"

if ! command -v uv &>/dev/null; then
  echo "  uv not found. Installing..."
  curl -LsSf https://astral.sh/uv/install.sh | sh
  # Add to PATH for the rest of this script
  export PATH="$HOME/.local/bin:$PATH"
  command -v uv &>/dev/null || die "uv install succeeded but binary not found — open a new shell and retry."
fi
ok "uv $(uv --version)"

# ── 3. Python virtualenv via uv ───────────────────────────────────────────────
step "Setting up Python virtualenv (.venv)"

# Creates .venv at the repo root. uv picks the best available Python >= 3.9.
# The extension uses the abi3 stable ABI so it runs on Python 3.9 – 3.14+.
uv venv .venv --python ">=3.9" --quiet
ok ".venv ready ($(.venv/bin/python --version))"

# ── 4. Python dev tools ───────────────────────────────────────────────────────
step "Installing Python dev tools"

# pyproject.toml [tool.uv] dev-dependencies drives this install.
uv sync --dev --quiet
ok "maturin, pytest, ruff, bandit installed"

# ── 5. Build Rust workspace ───────────────────────────────────────────────────
step "Building Rust workspace"
cargo build --workspace --exclude opssignal-py
ok "cargo build complete"

# ── 6. Build Python extension ─────────────────────────────────────────────────
step "Building Python extension (maturin develop)"
(
  cd python
  source "../.venv/bin/activate"
  maturin develop --quiet
)
ok "opssignal._native built (abi3 — works on Python 3.9+)"

# ── 7. Run test suite ─────────────────────────────────────────────────────────
step "Running Rust tests"
cargo test --workspace --exclude opssignal-py
ok "Rust tests passed"

step "Running Python tests"
source .venv/bin/activate
pytest --tb=short -q
ok "Python tests passed"

# ── 8. Git hooks ──────────────────────────────────────────────────────────────
step "Installing git hooks"
bash scripts/setup-hooks.sh
ok "hooks configured (core.hooksPath = .githooks)"

# ── 9. ast-grep (optional) ────────────────────────────────────────────────────
step "Checking for ast-grep (optional)"

if command -v sg &>/dev/null; then
  ok "sg already installed: $(command -v sg)"
elif command -v npm &>/dev/null; then
  echo "  npm found — installing @ast-grep/cli..."
  npm install -g @ast-grep/cli --silent
  ok "ast-grep installed: $(command -v sg 2>/dev/null || echo 'reload shell to pick it up')"
else
  warn "npm not found — ast-grep will be skipped in the pre-commit hook."
  warn "Install Node.js (https://nodejs.org) then run: npm install -g @ast-grep/cli"
fi

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}Setup complete.${NC}"
echo ""
echo "  Activate the Python venv:   source .venv/bin/activate"
echo "  Rebuild Python extension:   cd python && maturin develop"
echo "  Run all tests:              cargo test --workspace && pytest"
echo "  Lint:                       cargo clippy --workspace -- -D warnings"
echo "                              ruff check python/opssignal python/tests"
echo ""
echo "  The pre-commit hook runs fmt, clippy, ast-grep, and ruff automatically."
echo "  See CONTRIBUTING.md for project conventions."
