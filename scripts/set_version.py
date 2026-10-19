#!/usr/bin/env python3
"""Write one version into every file that carries it.

The release workflow runs this with the version from the pushed tag
before it builds anything, so a release needs no version-bump commit:

    python3 scripts/set_version.py 0.2.0     # or v0.2.0

It rewrites the Cargo workspace version, opssignal-cli's exact pin on
opssignal-core, the four pyproject.toml versions, opssignal's extras
pins on its add-ons, and opssignal.__version__. Run
scripts/check_versions.py afterwards to confirm they all agree.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Plain X.Y.Z: the one form that is the same string in Cargo (SemVer)
# and Python (PEP 440).
VERSION = re.compile(r"\d+\.\d+\.\d+")

# (file, pattern, count). Each pattern's groups 1 and 2 surround the
# version; count is how many matches the file must have.
EDITS = [
    ("Cargo.toml", r'(\[workspace\.package\][^\[]*?\nversion = ")[^"]+(")', 1),
    ("crates/cli/Cargo.toml", r'(opssignal-core = \{[^}\n]*version = "=)[^"]+(")', 1),
    ("python/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1),
    ("python/pyproject.toml", r'("opssignal-(?:aws|gcp|azure)==)[^"]+(")', None),
    ("python/addons/aws/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1),
    ("python/addons/gcp/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1),
    ("python/addons/azure/pyproject.toml", r'(\[project\][^\[]*?\nversion = ")[^"]+(")', 1),
    ("python/opssignal/__init__.py", r'(?m)(^__version__ = ")[^"]+(")', 1),
]


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: set_version.py X.Y.Z")
    version = sys.argv[1].removeprefix("v")
    if not VERSION.fullmatch(version):
        sys.exit(f"{sys.argv[1]!r} is not a vX.Y.Z version")

    for path, pattern, count in EDITS:
        file = ROOT / path
        text, n = re.subn(pattern, rf"\g<1>{version}\g<2>", file.read_text())
        if n == 0 or (count is not None and n != count):
            sys.exit(f"{path}: expected {count or 'some'} version(s) to set, found {n}")
        file.write_text(text)
        print(f"{path}: {n} set to {version}")


if __name__ == "__main__":
    main()
