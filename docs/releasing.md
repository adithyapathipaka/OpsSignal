# Releasing

One tag publishes everything. `.github/workflows/release.yml` builds the
packages below, tests the wheels, then uploads them.

| Registry  | Package           | Built from                         | Installed with                    |
|-----------|-------------------|------------------------------------|-----------------------------------|
| PyPI      | `opssignal`       | `python/` + `crates/python`        | `pip install opssignal`           |
| PyPI      | `opssignal-aws`   | `python/addons/aws` + `crates/python-aws`     | `pip install 'opssignal[aws]'`   |
| PyPI      | `opssignal-gcp`   | `python/addons/gcp` + `crates/python-gcp`     | `pip install 'opssignal[gcp]'`   |
| PyPI      | `opssignal-azure` | `python/addons/azure` + `crates/python-azure` | `pip install 'opssignal[azure]'` |
| crates.io | `opssignal-core`  | `crates/core`                      | `cargo add opssignal-core`        |
| crates.io | `opssignal-cli`   | `crates/cli`                       | `cargo install opssignal-cli`     |

`opssignal[all]` installs all three add-ons. `opssignal-ffi` and the
`opssignal-py*` crates are `publish = false`: the Python crates only exist
inside wheels, and C users build the library from source.

Wheels are abi3, so one wheel per platform covers CPython 3.9 and later:
Linux x86_64 and aarch64 (manylinux and musllinux) and Windows x86_64.
Each package also gets an sdist. macOS wheels are turned off for now
because hosted macOS runners are often unavailable; on macOS, pip builds
from the sdist, which needs a Rust toolchain. To turn them back on,
uncomment the two macOS lines in the `wheels` matrix in `release.yml` and
add `macos-latest` to the smoke test.

## One-time setup

Nothing is published until this is done. PyPI needs no stored secret at all;
crates.io needs a token for the first release only.

### GitHub environments

In **Settings > Environments**, create five environments: `pypi`,
`pypi-aws`, `pypi-gcp`, `pypi-azure` and `crates-io`. Add yourself as a
**required reviewer** on each, so a pushed tag waits for your click before
anything is uploaded (the Actions run lets you approve them all at once).

### PyPI: trusted publishing

The projects don't exist yet, so register a *pending* publisher for each one
at <https://pypi.org/manage/account/publishing/> (do the same on
<https://test.pypi.org> if you want a rehearsal). Add four entries, one per
project, each with its own environment name:

| PyPI project name | Environment name |
|-------------------|------------------|
| `opssignal`       | `pypi`           |
| `opssignal-aws`   | `pypi-aws`       |
| `opssignal-gcp`   | `pypi-gcp`       |
| `opssignal-azure` | `pypi-azure`     |

All four use owner `adithyapathipaka`, repository `OpsSignal` and workflow
`release.yml`. The environments must differ: a trusted-publishing token can
create only the one project whose pending publisher it matched, so four
publishers with the same environment fail with "Non-user identities cannot
create new projects".

A pending publisher does not reserve the name: whoever uploads first gets
it. The first successful release turns each one into a normal publisher.

### crates.io: token first, then trusted publishing

crates.io only allows trusted publishing for crates that already exist.

1. Sign in to <https://crates.io> with GitHub and verify your email address
   (publishing is refused without it).
2. Create an API token at <https://crates.io/settings/tokens> with the
   `publish-new` and `publish-update` scopes, limited to the crates
   `opssignal-core` and `opssignal-cli`, with a short expiry.
3. Save it as the secret `CARGO_REGISTRY_TOKEN` on the `crates-io`
   environment.
4. After the first release, open each crate's **Settings > Trusted
   Publishing** on crates.io and add owner `adithyapathipaka`, repository
   `OpsSignal`, workflow `release.yml`, environment `crates-io`. Then delete
   the `CARGO_REGISTRY_TOKEN` secret and revoke the token; the workflow
   switches to trusted publishing on its own when the secret is gone.

## Cutting a release

1. Tag the commit on `main` you want to release and push the tag:

    ```bash
    git tag v0.2.0
    git push origin v0.2.0
    ```

    The tag sets the version. Before building, the workflow runs
    `scripts/set_version.py` to write `0.2.0` into `Cargo.toml`
    (`[workspace.package]` and the `opssignal-core` pin in `crates/cli`),
    the four `pyproject.toml` files, the extras pins in
    `python/pyproject.toml`, and `opssignal.__version__`, then
    `scripts/check_versions.py` confirms they all agree. You don't need a
    version-bump commit.

    Tags are `vX.Y.Z` for a release, or `vX.Y.Z-alpha.N`, `-beta.N` or
    `-rc.N` for a pre-release; anything else stops the run before it
    builds. A pre-release keeps that form on crates.io (`1.0.0-beta.1`)
    and uses Python's spelling on PyPI (`1.0.0b1`). Neither registry gives
    it to users by default: they ask for it with
    `pip install --pre opssignal` or `opssignal-core = "1.0.0-beta.1"`.
    The GitHub release is marked as a pre-release.

    The change is made only inside the release run. The files on `main`
    keep their old version, so `main` doesn't show which version was last
    released; the tags do.

2. Approve the `pypi-aws`, `pypi-gcp`, `pypi-azure`, `pypi` and
   `crates-io` deployments in the Actions run.

The run uploads the three add-ons first, then `opssignal` (its extras pin
the add-ons, so they must exist first), then `opssignal-core` and
`opssignal-cli`, and finally creates a GitHub release with every wheel and
sdist attached.

To build and test everything without publishing, run the workflow by hand
from **Actions > Release > Run workflow**. Pull requests don't run it, to
save Actions minutes, so do this before tagging if a change touched the
packaging (a `pyproject.toml`, `Cargo.toml` or `release.yml`); otherwise a
packaging mistake only shows up when the tag runs, and stops it before
anything is uploaded.

## If a release fails halfway

Registry uploads can't be undone, and a version number can never be reused
on either registry, even after it is yanked or deleted.

- **Before any upload** (a build or smoke test failed): fix it, delete the
  tag, and tag again.
- **After some uploads**: use **Re-run failed jobs** on the same run. Both
  publish jobs skip anything already uploaded, so a re-run picks up where
  it stopped. If the fix needs a code change, tag the next patch
  version and release that instead. Yank a broken version with `cargo yank`
  or PyPI's **Yank** button rather than deleting it.
