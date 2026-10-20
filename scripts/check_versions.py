#!/usr/bin/env python3
"""Check that every published package carries the same version.

The crates, the opssignal wheel, its add-on wheels and the add-on pins
in opssignal's extras all move together. A release is cut by tagging
vX.Y.Z; pass the tag to also check it matches:

    python3 scripts/check_versions.py            # packages agree
    python3 scripts/check_versions.py v0.1.0     # ... and match the tag

The Python files must hold the PEP 440 spelling of the Cargo version
(1.0.0-beta.1 there is 1.0.0b1 here); see set_version.py.
"""

import re
import sys
from pathlib import Path

import tomllib
from set_version import python_version

ROOT = Path(__file__).resolve().parent.parent
PYPROJECTS = [
    "python/pyproject.toml",
    "python/addons/aws/pyproject.toml",
    "python/addons/gcp/pyproject.toml",
    "python/addons/azure/pyproject.toml",
]
CLOUDS = ("aws", "gcp", "azure")


def load(path):
    with open(ROOT / path, "rb") as f:
        return tomllib.load(f)


def main():
    expected = load("Cargo.toml")["workspace"]["package"]["version"]
    try:
        expected_py = python_version(expected)
    except ValueError as e:
        sys.exit(f"Cargo.toml [workspace.package]: {e}")
    # (where, found, expected)
    found = [("Cargo.toml [workspace.package]", expected, expected)]

    for path in PYPROJECTS:
        found.append((path, load(path)["project"]["version"], expected_py))

    init = (ROOT / "python/opssignal/__init__.py").read_text()
    m = re.search(r'^__version__ = "([^"]+)"', init, re.M)
    found.append(("opssignal.__version__", m.group(1) if m else "", expected_py))

    # opssignal-cli pins opssignal-core exactly, so both publish together.
    core_dep = load("crates/cli/Cargo.toml")["dependencies"]["opssignal-core"]
    pin = core_dep.get("version", "").lstrip("=")
    found.append(("crates/cli opssignal-core dependency", pin, expected))

    extras = load("python/pyproject.toml")["project"]["optional-dependencies"]
    for extra, reqs in extras.items():
        for req in reqs:
            m = re.fullmatch(rf"(opssignal-(?:{'|'.join(CLOUDS)}))==(\S+)", req)
            if not m:
                sys.exit(f"python/pyproject.toml: extra [{extra}] pin {req!r} is not name==version")
            found.append((f"opssignal[{extra}] -> {m.group(1)}", m.group(2), expected_py))

    if len(sys.argv) > 1:
        found.append(("git tag", sys.argv[1].removeprefix("v"), expected))

    wrong = [(where, v, want) for where, v, want in found if v != want]
    if wrong:
        print(f"Expected version {expected} (Python: {expected_py}) everywhere, but:")
        for where, v, want in wrong:
            print(f"  {where}: {v or '(missing)'}, expected {want}")
        sys.exit(1)
    print(f"All {len(found)} versions are {expected}.")


if __name__ == "__main__":
    main()
