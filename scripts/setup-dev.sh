#!/usr/bin/env bash
# setup-dev.sh — one-time dev environment bootstrap for new contributors.
#
# Run from the repo root:
#   bash scripts/setup-dev.sh            # prompts: local or Docker?
#   bash scripts/setup-dev.sh --local    # local Rust + Python env
#   bash scripts/setup-dev.sh --docker   # Docker image only
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

# ── Mode selection ─────────────────────────────────────────────────────────────
MODE="${1:-}"

if [[ -z "$MODE" ]]; then
  echo ""
  echo -e "${BOLD}How would you like to develop?${NC}"
  echo ""
  echo "  1) local   — install Rust, Python (uv), build natively on this machine"
  echo "  2) docker  — build the Docker dev image; no local Rust/Python required"
  echo ""
  read -rp "Enter 1 or 2 [1]: " choice
  case "${choice:-1}" in
    1) MODE="--local"  ;;
    2) MODE="--docker" ;;
    *) die "Invalid choice. Run again and enter 1 or 2." ;;
  esac
fi

case "$MODE" in
  --local)  ;;
  --docker) ;;
  *) die "Unknown flag '$MODE'. Use --local or --docker." ;;
esac

# ══════════════════════════════════════════════════════════════════════════════
# LOCAL setup
# ══════════════════════════════════════════════════════════════════════════════
if [[ "$MODE" == "--local" ]]; then

# ── 1. Rust ───────────────────────────────────────────────────────────────────
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
  export PATH="$HOME/.local/bin:$PATH"
  command -v uv &>/dev/null || die "uv install succeeded but binary not in PATH — open a new shell and retry."
fi
ok "uv $(uv --version)"

# ── 3. Python virtualenv ──────────────────────────────────────────────────────
step "Setting up Python virtualenv (.venv)"

uv venv .venv --python ">=3.10" --quiet
ok ".venv ready ($(.venv/bin/python --version))"

# ── 4. Python dev tools ───────────────────────────────────────────────────────
step "Installing Python dev tools"

uv sync --dev --quiet
ok "dev dependencies installed (maturin, pytest, ruff, bandit, graphifyy)"

# ── 5. Build Rust workspace ───────────────────────────────────────────────────
step "Building Rust workspace"

cargo build --workspace --exclude opssignal-py
ok "cargo build complete"

# ── 6. Build Python extension ─────────────────────────────────────────────────
step "Building Python extension (maturin develop)"

(cd python && source "../.venv/bin/activate" && maturin develop --quiet)
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
  ok "ast-grep installed"
else
  warn "npm not found — ast-grep skipped in pre-commit hook."
  warn "Install Node.js then run: npm install -g @ast-grep/cli"
fi

# ── Done (local) ──────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}Local setup complete.${NC}"
echo ""
echo "  source .venv/bin/activate               # activate Python venv"
echo "  cargo build --workspace                 # Rust build"
echo "  cd python && maturin develop            # rebuild Python extension"
echo "  cargo test --workspace && pytest        # run all tests"
echo "  bash scripts/check.sh                   # full CI check suite"
echo ""
echo "  The pre-commit hook runs fmt, clippy, ast-grep, and ruff automatically."
echo "  See CONTRIBUTING.md for project conventions."

fi  # end --local

# ══════════════════════════════════════════════════════════════════════════════
# DOCKER setup
# ══════════════════════════════════════════════════════════════════════════════
if [[ "$MODE" == "--docker" ]]; then

# ── 1. Docker ─────────────────────────────────────────────────────────────────
step "Checking Docker"

if ! command -v docker &>/dev/null; then
  die "docker not found. Install Docker Desktop: https://docs.docker.com/get-started/get-docker/"
fi

if ! docker info &>/dev/null 2>&1; then
  die "Docker daemon is not running. Start Docker Desktop and retry."
fi
ok "Docker $(docker --version | awk '{print $3}' | tr -d ',')"

# ── 2. Build dev image ────────────────────────────────────────────────────────
step "Building Docker dev image"

echo "  This takes a few minutes the first time (downloads Rust + Node base layers)."
docker compose build
ok "opssignal-dev image ready"

# ── 3. Git hooks (host-side, for committing from the host) ────────────────────
step "Installing git hooks"

bash scripts/setup-hooks.sh
ok "hooks configured (pre-commit runs on the host before each commit)"

# ── Done (Docker) ─────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}Docker setup complete.${NC}"
echo ""
echo "  docker compose run --rm dev              # interactive shell (all tools)"
echo "  docker compose run --rm dev bash scripts/check.sh  # full check suite"
echo "  docker compose run --rm test             # same as above, one-liner"
echo ""
echo "  Inside the container, first-run bootstrap:"
echo "    uv sync --dev && cd python && uv run maturin develop && cd .."
echo ""
echo "  See CONTRIBUTING.md for project conventions."

fi  # end --docker
