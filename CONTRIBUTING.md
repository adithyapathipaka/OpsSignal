# Contributing to opssignal

Thanks for considering a contribution. This project is solo/small-team
maintained right now, so please read this before opening a large PR — it'll
save both of us time.

## License and project model (read this first)

`opssignal` (this repository) is Apache-2.0 licensed, full stop — anyone can
use, modify, fork, and build on it, including commercially. There is no
contributor license agreement (CLA); by opening a PR you license your
contribution under the same Apache-2.0 terms as the rest of the repo, same
as any other Apache-2.0 project.

Separately, the maintainer plans to build and sell a **hosted control
plane** (centralized policy management, RBAC/SSO, delivery analytics, AI
incident correlation) as a closed-source product that reads from the same
storage format this SDK writes to. That hosted product is not in this
repository, is not under any open-source license, and contributions here do
not grant any special rights to it beyond what Apache-2.0 already gives
everyone — including the right for you to build a competing hosted product
yourself, if you want to. We're stating this plainly up front so there are
no surprises later; this is a deliberate "open-core" structure, not a
bait-and-switch.

## Before you start

- **Check the roadmap in README.md.** Java and Go bindings are explicitly
  deferred until there's real demonstrated demand — a PR adding either will
  likely sit unreviewed for a while unless you're also committing to help
  maintain it long-term. Open an issue to discuss first.
- **Core logic belongs in `crates/core` only.** `crates/ffi`, `crates/python`,
  and the CLI must stay thin — see `docs/ARCHITECTURE.md` for why.
- **New providers/sinks**: implement the `Provider` / `SignalSink` trait.
  You should not need to touch `routing.rs` or `lib.rs` to add one.

## Required for any PR touching `crates/ffi` or `crates/python`

Every FFI entry point must be wrapped in the project's panic-safety pattern
(`guarded(...)` in `crates/ffi/src/lib.rs`). PRs that add a raw
`extern "C" fn` without this wrapper will be asked to change it before
review continues — this is a hard project rule, not a style preference.

## Development setup

Run the bootstrap script once after cloning:

```bash
bash scripts/setup-dev.sh
```

This verifies Rust >= 1.75 and Python >= 3.9, creates `.venv` at the repo
root, installs all dev tools (maturin, pytest, ruff, bandit, ast-grep),
builds the workspace, runs the full test suite, and wires up the git hooks.
Safe to re-run if your environment gets out of sync.

The script requires **uv** (installed automatically if missing) and Rust >= 1.75.
The Python extension is built with the `abi3` stable ABI so it runs on
Python 3.9 through 3.14+ without recompiling.

Day-to-day commands:

```bash
source .venv/bin/activate                          # activate Python venv (repo root)
cargo build --workspace --exclude opssignal-py     # Rust build
cd python && maturin develop                       # rebuild Python extension
cargo test --workspace                             # Rust tests
pytest                                             # Python tests (from repo root)
```

## Commit / PR conventions

- One logical change per PR.
- Add or update tests for any behavior change — especially routing
  fallback behavior and FFI panic handling, both of which have existing
  test coverage you should extend, not replace.
- Update `docs/ARCHITECTURE.md` if you're changing a documented decision
  (not just adding a new provider/sink, which doesn't need an architecture
  doc change).

## Security

If you find a credential-handling or FFI memory-safety issue, please do not
open a public issue — see `SECURITY.md` for responsible disclosure.
