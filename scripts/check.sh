#!/usr/bin/env bash
# check.sh — run the full CI check suite locally or inside the Docker dev container.
#
# Usage:
#   bash scripts/check.sh                  # from repo root (native or in container)
#   docker compose run --rm dev bash scripts/check.sh
#   docker compose run --rm test           # calls this script automatically

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

BOLD='\033[1m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

step() { echo -e "\n${CYAN}${BOLD}==>${NC}${BOLD} $*${NC}"; }
ok()   { echo -e "  ${GREEN}ok${NC}  $*"; }

step "uv sync"
uv sync --dev --quiet
ok "dev dependencies up to date"

step "maturin develop"
(cd python && uv run maturin develop --quiet)
ok "Python extension built"

step "cargo test"
cargo test --workspace --exclude opssignal-py
ok "Rust tests passed"

step "pytest"
uv run pytest --tb=short -q
ok "Python tests passed"

step "ruff"
uv run ruff check python/opssignal python/tests
uv run ruff format --check python/opssignal python/tests
ok "Python lint clean"

step "ast-grep"
if command -v sg &>/dev/null; then
  sg scan --config sgconfig.yml
  ok "structural lint clean"
else
  echo -e "  \033[1;33mskip\033[0m  sg not found — ast-grep skipped"
fi

echo ""
echo -e "${GREEN}${BOLD}All checks passed.${NC}"
