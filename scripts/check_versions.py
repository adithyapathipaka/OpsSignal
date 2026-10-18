#!/usr/bin/env python3
"""Check that every published package carries the same version.

The crates, the opssignal wheel, its add-on wheels and the add-on pins
in opssignal's extras all move together. A release is cut by tagging
vX.Y.Z; pass the tag to also check it matches:

    python3 scripts/check_versions.py            # packages agree
    python3 scripts/check_versions.py v0.1.0     # ... and match the tag
"""

import re
import sys
from pathlib import Path

import tomllib

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
    found = {"Cargo.toml [workspace.package]": expected}

    for path in PYPROJECTS:
        found[path] = load(path)["project"]["version"]

    init = (ROOT / "python/opssignal/__init__.py").read_text()
    m = re.search(r'^__version__ = "([^"]+)"', init, re.M)
    found["opssignal.__version__"] = m.group(1) if m else ""

    # opssignal-cli pins opssignal-core exactly, so both publish together.
    core_dep = load("crates/cli/Cargo.toml")["dependencies"]["opssignal-core"]
    found["crates/cli opssignal-core dependency"] = core_dep.get("version", "").lstrip("=")

    extras = load("python/pyproject.toml")["project"]["optional-dependencies"]
    for extra, reqs in extras.items():
        for req in reqs:
            m = re.fullmatch(rf"(opssignal-(?:{'|'.join(CLOUDS)}))==(\S+)", req)
            if not m:
                sys.exit(f"python/pyproject.toml: extra [{extra}] pin {req!r} is not name==version")
            found[f"opssignal[{extra}] -> {m.group(1)}"] = m.group(2)

    if len(sys.argv) > 1:
        found["git tag"] = sys.argv[1].removeprefix("v")

    wrong = {where: v for where, v in found.items() if v != expected}
    if wrong:
        print(f"Expected version {expected} everywhere, but:")
        for where, v in wrong.items():
            print(f"  {where}: {v or '(missing)'}")
        sys.exit(1)
    print(f"All {len(found)} versions are {expected}.")


if __name__ == "__main__":
    main()
