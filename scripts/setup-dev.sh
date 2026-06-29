#!/usr/bin/env bash
# setup-dev.sh — one-time dev environment bootstrap for new contributors.
#
# Run from the repo root:
#   bash scripts/setup-dev.sh
#
# What it does:
#   1. Verifies Rust >= 1.75 with rustfmt + clippy components
#   2. Verifies Python >= 3.9
#   3. Creates python/.venv and installs Python dev tools
#   4. Builds the Rust workspace and Python extension (maturin develop)
#   5. Runs the full test suite to confirm everything works
#   6. Installs git hooks (cargo fmt --check, clippy, ast-grep, ruff)
#   7. Installs ast-grep (optional, via npm)
#
# Safe to re-run — all steps are idempotent.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# ── Colour helpers ─────────────────────────────────────────────────────────────
BOLD='\033[1m'
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

step()  { echo -e "\n${CYAN}${BOLD}==>${NC}${BOLD} $*${NC}"; }
ok()    { echo -e "  ${GREEN}ok${NC}  $*"; }
warn()  { echo -e "  ${YELLOW}warn${NC}  $*"; }
die()   { echo -e "\n  ${RED}error${NC}  $*\n" >&2; exit 1; }

# ── 1. Rust ────────────────────────────────────────────────────────────────────
step "Checking Rust toolchain"

if ! command -v rustup &>/dev/null; then
  echo "  rustup not found. Installing..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
  source "$HOME/.cargo/env"
fi

# Make sure cargo is on PATH (handles fresh rustup installs)
if ! command -v cargo &>/dev/null; then
  [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
  command -v cargo &>/dev/null || die "cargo still not found after sourcing ~/.cargo/env — open a new shell and retry."
fi

RUST_VERSION=$(rustc --version | awk '{print $2}')
RUST_MAJOR=$(echo "$RUST_VERSION" | cut -d. -f1)
RUST_MINOR=$(echo "$RUST_VERSION" | cut -d. -f2)
REQUIRED_MINOR=75  # matches workspace.package.rust-version in Cargo.toml

if [ "$RUST_MAJOR" -lt 1 ] || { [ "$RUST_MAJOR" -eq 1 ] && [ "$RUST_MINOR" -lt "$REQUIRED_MINOR" ]; }; then
  die "Rust $RUST_VERSION found but >= 1.${REQUIRED_MINOR} required. Run: rustup update stable"
fi
ok "rustc $RUST_VERSION"

# Ensure rustfmt and clippy are present (may be missing in minimal profiles)
rustup component add rustfmt clippy --quiet
ok "rustfmt + clippy installed"

# ── 2. Python ──────────────────────────────────────────────────────────────────
step "Checking Python"

PYTHON=""
for candidate in python3 python3.12 python3.11 python3.10 python3.9; do
  if command -v "$candidate" &>/dev/null; then
    PY_VER=$("$candidate" -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
    PY_MAJOR=$(echo "$PY_VER" | cut -d. -f1)
    PY_MINOR=$(echo "$PY_VER" | cut -d. -f2)
    if [ "$PY_MAJOR" -ge 3 ] && [ "$PY_MINOR" -ge 9 ]; then
      PYTHON="$candidate"
      break
    fi
  fi
done

[ -n "$PYTHON" ] || die "Python >= 3.9 not found. Install it via pyenv, asdf, or your system package manager."
ok "$PYTHON $PY_VER"

# ── 3. Python virtualenv + dev tools ──────────────────────────────────────────
step "Setting up Python virtualenv (python/.venv)"

VENV_DIR="python/.venv"

if [ ! -d "$VENV_DIR" ]; then
  "$PYTHON" -m venv "$VENV_DIR"
  ok "created $VENV_DIR"
else
  ok "$VENV_DIR already exists"
fi

# Use the venv's pip for all installs
VENV_PIP="$VENV_DIR/bin/pip"
VENV_PYTHON="$VENV_DIR/bin/python"

"$VENV_PIP" install --quiet --upgrade pip

step "Installing Python dev tools into venv"
"$VENV_PIP" install --quiet "maturin>=1.5,<2.0" pytest ruff bandit
ok "maturin, pytest, ruff, bandit installed"

# ── 4. Build Rust workspace ────────────────────────────────────────────────────
step "Building Rust workspace"
# Exclude opssignal-py during initial build; it is built via maturin below.
cargo build --workspace --exclude opssignal-py
ok "cargo build complete"

# ── 5. Build Python extension (maturin develop) ───────────────────────────────
step "Building Python extension via maturin"
(
  cd python
  source ".venv/bin/activate"
  maturin develop --quiet
)
ok "opssignal._native built and installed into venv"

# ── 6. Run test suite ──────────────────────────────────────────────────────────
step "Running Rust tests"
cargo test --workspace --exclude opssignal-py
ok "Rust tests passed"

step "Running Python tests"
(
  cd python
  source ".venv/bin/activate"
  pytest --tb=short -q
)
ok "Python tests passed"

# ── 7. Git hooks ───────────────────────────────────────────────────────────────
step "Installing git hooks"
bash scripts/setup-hooks.sh
ok "hooks configured (core.hooksPath = .githooks)"

# ── 8. ast-grep (optional) ────────────────────────────────────────────────────
step "Checking for ast-grep (optional)"

if command -v sg &>/dev/null; then
  ok "sg already installed: $(command -v sg)"
elif command -v npm &>/dev/null; then
  echo "  npm found — installing @ast-grep/cli..."
  npm install -g @ast-grep/cli --silent
  ok "ast-grep installed: $(command -v sg 2>/dev/null || echo 'reload your shell to pick it up')"
else
  warn "npm not found — ast-grep will be skipped in the pre-commit hook."
  warn "Install Node.js (https://nodejs.org) then run: npm install -g @ast-grep/cli"
fi

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}Setup complete.${NC}"
echo ""
echo "  Activate the Python venv:   source python/.venv/bin/activate"
echo "  Build after Rust changes:   cargo build --workspace --exclude opssignal-py"
echo "  Rebuild Python extension:   cd python && maturin develop"
echo "  Run all tests:              cargo test --workspace && cd python && pytest"
echo "  Lint:                       cargo clippy --workspace -- -D warnings"
echo "                              ruff check python/"
echo ""
echo "  The pre-commit hook runs fmt, clippy, ast-grep, and ruff automatically."
echo "  See CONTRIBUTING.md for project conventions."
