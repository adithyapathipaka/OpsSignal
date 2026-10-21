#!/usr/bin/env python3
"""Write one version into every file that carries it.

The release workflow runs this with the version from the pushed tag
before it builds anything, so a release needs no version-bump commit:

    python3 scripts/set_version.py 0.2.0          # or v0.2.0
    python3 scripts/set_version.py 1.0.0-beta.1   # pre-release

It rewrites the Cargo workspace version, opssignal-cli's exact pin on
opssignal-core, the four pyproject.toml versions, opssignal's extras
pins on its add-ons, and opssignal.__version__. It also points the docs
links that PyPI shows (the Documentation URL and the package READMEs) at
that version's copy of the docs site, so a release links to the docs
that match it rather than "latest". Run
scripts/check_versions.py afterwards to confirm they all agree.

A pre-release is X.Y.Z-alpha.N, -beta.N or -rc.N. Cargo takes that
SemVer form as is; the Python files get the PEP 440 spelling of the
same version (1.0.0-beta.1 becomes 1.0.0b1).
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# X.Y.Z, optionally with a pre-release that has a PEP 440 equivalent.
VERSION = re.compile(r"(\d+\.\d+\.\d+)(?:-(alpha|beta|rc)\.(\d+))?")
PEP440_PRE = {"alpha": "a", "beta": "b", "rc": "rc"}

# (file, pattern, count, python). Each pattern's groups 1 and 2 surround
# the version; count is how many matches the file must have; python
# says the file takes the PEP 440 spelling.
EDITS = [
    ("Cargo.toml", r'(\[workspace\.package\][^\[]*?\nversion = ")[^"]+(")', 1, False),
    ("crates/cli/Cargo.toml", r'(opssignal-core = \{[^}\n]*version = "=)[^"]+(")', 1, False),
    ("python/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1, True),
    ("python/pyproject.toml", r'("opssignal-(?:aws|gcp|azure)==)[^"]+(")', None, True),
    ("python/addons/aws/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1, True),
    ("python/addons/gcp/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1, True),
    ("python/addons/azure/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1, True),
    ("python/opssignal/__init__.py", r'(?m)(^__version__ = ")[^"]+(")', 1, True),
]

# Docs links PyPI shows. On main they point at the site root or latest/;
# a release rewrites them to its own version (the mike version name is
# the Cargo spelling). Any version already in the URL is replaced too.
DOCS_URL = re.compile(
    r"(https://adithyapathipaka\.github\.io/OpsSignal/)"
    r"(?:(?:latest|\d+\.\d+\.\d+(?:-(?:alpha|beta|rc)\.\d+)?)/)?"
)
DOCS_FILES = [
    "python/pyproject.toml",
    "python/addons/aws/pyproject.toml",
    "python/addons/gcp/pyproject.toml",
    "python/addons/azure/pyproject.toml",
    "python/README.md",
    "python/addons/aws/README.md",
    "python/addons/gcp/README.md",
    "python/addons/azure/README.md",
]


def python_version(version):
    """The PEP 440 spelling of a Cargo version: 1.0.0-beta.1 -> 1.0.0b1."""
    m = VERSION.fullmatch(version)
    if not m:
        raise ValueError(f"{version!r} is not X.Y.Z or X.Y.Z-(alpha|beta|rc).N")
    release, pre, n = m.groups()
    return release + (f"{PEP440_PRE[pre]}{n}" if pre else "")


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: set_version.py X.Y.Z[-(alpha|beta|rc).N]")
    cargo = sys.argv[1].removeprefix("v")
    try:
        python = python_version(cargo)
    except ValueError as e:
        sys.exit(str(e))

    for path, pattern, count, is_python in EDITS:
        version = python if is_python else cargo
        file = ROOT / path
        text, n = re.subn(pattern, rf"\g<1>{version}\g<2>", file.read_text())
        if n == 0 or (count is not None and n != count):
            sys.exit(f"{path}: expected {count or 'some'} version(s) to set, found {n}")
        file.write_text(text)
        print(f"{path}: {n} set to {version}")

    for path in DOCS_FILES:
        file = ROOT / path
        text, n = DOCS_URL.subn(rf"\g<1>{cargo}/", file.read_text())
        if n == 0:
            sys.exit(f"{path}: expected a docs link to set, found none")
        file.write_text(text)
        print(f"{path}: {n} docs link(s) set to {cargo}")


if __name__ == "__main__":
    main()
