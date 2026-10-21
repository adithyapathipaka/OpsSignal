#!/usr/bin/env python3
"""Write the GitHub release notes for a tag from CHANGELOG.md.

The release workflow runs this on a vX.Y.Z tag and uses its output as the
release body, so the notes are the curated changelog entry rather than a
list of every merged PR:

    python3 scripts/release_notes.py v1.0.0-beta.2 > notes.md
    python3 scripts/release_notes.py v1.0.0-beta.2 v1.0.0-beta.1  # with compare link

It exits non-zero when CHANGELOG.md has no "## [X.Y.Z]" section for the
tag, so a release can't go out without one.
"""

import re
import sys
from pathlib import Path

from set_version import python_version

ROOT = Path(__file__).resolve().parent.parent
REPO = "https://github.com/adithyapathipaka/OpsSignal"
DOCS = "https://adithyapathipaka.github.io/OpsSignal"


def changelog_section(version):
    """The body of the "## [version]" section, without its heading."""
    text = (ROOT / "CHANGELOG.md").read_text()
    heading = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n", text, re.M)
    if not heading:
        raise ValueError(f'CHANGELOG.md has no "## [{version}]" section')
    rest = text[heading.end() :]
    # The section ends at the next version heading or the link references.
    end = re.search(r"^(## \[|\[[^\]]+\]: )", rest, re.M)
    body = rest[: end.start() if end else len(rest)].strip()
    if not body:
        raise ValueError(f'CHANGELOG.md section "## [{version}]" is empty')
    return body


def notes(tag, previous=None):
    version = tag.removeprefix("v")
    python = python_version(version)
    pre = "-" in version
    pip = "pip install --pre" if pre else "pip install"
    lines = [
        changelog_section(version),
        "",
        "## Install",
        "",
        "```bash",
        f"{pip} 'opssignal=={python}'         # local and https:// configs",
        f"{pip} 'opssignal[all]=={python}'    # or [aws], [gcp], [azure] for one cloud",
        f"cargo install opssignal-cli --version {version}",
        "```",
        "",
    ]
    if pre:
        lines += [
            "> This is a pre-release: config keys, the Python API and the SQLite",
            "> schema may still change before the final release.",
            "",
        ]
    lines.append(f"**Documentation:** {DOCS}/{version}/")
    if previous:
        lines += ["", f"**Full changelog:** {REPO}/compare/{previous}...{tag}"]
    return "\n".join(lines) + "\n"


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit("usage: release_notes.py vX.Y.Z [previous-tag]")
    try:
        sys.stdout.write(notes(*sys.argv[1:]))
    except ValueError as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
