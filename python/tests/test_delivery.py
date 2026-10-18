"""End-to-end checks that notify() reaches the Rust SignalClient."""

import json
import os
import subprocess  # runs this interpreter only
import sys
import textwrap
import time
from pathlib import Path

from opssignal import notify


def _db_path() -> Path:
    return Path(os.environ["SIGNAL_CONFIG"]).parent / "signals.db"


def _rows_titled(title: str) -> list:
    # Read from another process: Python's sqlite3 and the extension's
    # bundled SQLite would otherwise share POSIX locks on the same file.
    db = _db_path()
    if not db.exists():
        return []
    result = _run_isolated(
        """
        import json, sqlite3, sys
        with sqlite3.connect(sys.argv[1]) as conn:
            rows = conn.execute(
                "SELECT source, severity, delivery_status, provider FROM signals WHERE title = ?",
                (sys.argv[2],),
            ).fetchall()
        print(json.dumps(rows))
        """,
        {},
        str(db),
        title,
    )
    if result.returncode != 0:
        return []
    return [tuple(r) for r in json.loads(result.stdout)]


def test_notify_delivers_to_console_and_records_the_signal():
    title = f"delivery check {time.time_ns()}"
    notify(source="pytest", event_type="check", title=title, severity="warning")

    deadline = time.monotonic() + 5
    while not _rows_titled(title) and time.monotonic() < deadline:
        time.sleep(0.05)

    assert _rows_titled(title) == [("pytest", "warning", "delivered", "console")]


def _run_isolated(code: str, env: dict, *args: str) -> subprocess.CompletedProcess:
    # A fresh interpreter, because the client is built once per process.
    return subprocess.run(  # noqa: S603 - fixed argv, no shell
        [sys.executable, "-c", textwrap.dedent(code), *args],
        env={**os.environ, **env},
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_missing_config_file_raises_runtime_error(tmp_path):
    result = _run_isolated(
        """
        from opssignal import notify
        try:
            notify(source="pytest", event_type="check", title="t")
        except RuntimeError as e:
            print("RuntimeError:", e)
        """,
        {"SIGNAL_CONFIG": str(tmp_path / "missing.yaml")},
    )
    assert result.returncode == 0, result.stderr
    assert "RuntimeError: opssignal is not configured" in result.stdout
    assert "missing.yaml" in result.stdout


def test_config_error_is_raised_on_every_call(tmp_path):
    bad = tmp_path / "signal.yaml"
    bad.write_text("providers:\n  slak: {}\n")
    result = _run_isolated(
        """
        from opssignal import notify
        for _ in range(2):
            try:
                notify(source="pytest", event_type="check", title="t")
            except RuntimeError:
                print("raised")
        """,
        {"SIGNAL_CONFIG": str(bad)},
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.split() == ["raised", "raised"]


def test_invalid_input_is_rejected_before_config_is_loaded(tmp_path):
    result = _run_isolated(
        """
        from opssignal import notify
        try:
            notify(source="", event_type="check", title="t")
        except ValueError as e:
            print("ValueError:", e)
        """,
        {"SIGNAL_CONFIG": str(tmp_path / "missing.yaml")},
    )
    assert result.returncode == 0, result.stderr
    expected = "ValueError: invalid signal: validation failed: source must not be empty"
    assert expected in result.stdout
