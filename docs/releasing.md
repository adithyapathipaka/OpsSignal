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
Linux x86_64 and aarch64 (manylinux and musllinux), macOS x86_64 and arm64,
and Windows x86_64. Each package also gets an sdist.

## One-time setup

Nothing is published until this is done. PyPI needs no stored secret at all;
crates.io needs a token for the first release only.

### GitHub environments

In **Settings > Environments**, create two environments, `pypi` and
`crates-io`. Add yourself as a **required reviewer** on both, so a pushed tag
waits for your click before anything is uploaded.

### PyPI: trusted publishing

The projects don't exist yet, so register a *pending* publisher for each one
at <https://pypi.org/manage/account/publishing/> (do the same on
<https://test.pypi.org> if you want a rehearsal). Add four entries, one per
project name, all with the same values:

| Field             | Value                                  |
|-------------------|----------------------------------------|
| PyPI project name | `opssignal`, `opssignal-aws`, `opssignal-gcp`, `opssignal-azure` |
| Owner             | `adithyapathipaka`                     |
| Repository name   | `OpsSignal`                            |
| Workflow name     | `release.yml`                          |
| Environment name  | `pypi`                                 |

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

1. Bump the version everywhere it appears and check they agree:

    ```bash
    python3 scripts/check_versions.py
    ```

    That covers `Cargo.toml` (`[workspace.package]` and the `opssignal-core`
    pin in `crates/cli`), the four `pyproject.toml` files, the extras pins in
    `python/pyproject.toml`, and `opssignal.__version__`.

2. Merge to `main`, then tag and push:

    ```bash
    git tag v0.1.0
    git push origin v0.1.0
    ```

3. Approve the `pypi` and `crates-io` deployments in the Actions run.

The run uploads the three add-ons first, then `opssignal` (its extras pin
the add-ons, so they must exist first), then `opssignal-core` and
`opssignal-cli`, and finally creates a GitHub release with every wheel and
sdist attached.

To build and test everything without publishing, run the workflow by hand
from **Actions > Release > Run workflow**. Pull requests that touch
packaging run it too.

## If a release fails halfway

Registry uploads can't be undone, and a version number can never be reused
on either registry, even after it is yanked or deleted.

- **Before any upload** (a build or smoke test failed): fix it, delete the
  tag, and tag again.
- **After some uploads**: use **Re-run failed jobs** on the same run. Both
  publish jobs skip anything already uploaded, so a re-run picks up where
  it stopped. If the fix needs a code change, bump to the next patch
  version and release that instead. Yank a broken version with `cargo yank`
  or PyPI's **Yank** button rather than deleting it.
