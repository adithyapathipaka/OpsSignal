"""End-to-end checks that notify() reaches the Rust SignalClient."""

import http.server
import json
import os
import subprocess  # runs this interpreter only
import sys
import textwrap
import threading
import time
from pathlib import Path

import pytest
from opssignal import flush, notify


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

    assert flush(5.0)
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
        from opssignal import flush, notify
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
        from opssignal import flush, notify
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
        from opssignal import flush, notify
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


def test_signal_config_lists_layered_files(tmp_path):
    db = tmp_path / "from-default.db"
    (tmp_path / "default.yaml").write_text(f'storage:\n  path: "{db}"\n')
    team = tmp_path / "teams" / "etl.yaml"
    team.parent.mkdir()
    team.write_text("extends: ../default.yaml\n")
    shared = tmp_path / "shared.yaml"
    shared.write_text("routes: []\n")

    result = _run_isolated(
        """
        from opssignal import flush, notify
        notify(source="pytest", event_type="check", title="t")
        print("NOTIFY-RETURNED")
        """,
        {"SIGNAL_CONFIG": f"{team},{shared}"},
    )

    assert result.returncode == 0, result.stderr
    assert "NOTIFY-RETURNED" in result.stdout, result.stdout
    # The sink path is only set in the inherited default.
    assert db.exists()


def test_flush_rejects_a_negative_timeout():
    with pytest.raises(ValueError, match="invalid timeout"):
        flush(-1)


class _SlowHook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        time.sleep(0.5)
        self.send_response(200)
        self.end_headers()

    def log_message(self, *args):
        pass


def test_signals_in_flight_at_exit_are_delivered(tmp_path):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _SlowHook)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    db = tmp_path / "signals.db"
    config = tmp_path / "signal.yaml"
    config.write_text(
        textwrap.dedent(
            f"""\
            providers:
              webhook:
                url: "http://127.0.0.1:{server.server_port}/hook"
            routes:
              - match: {{}}
                providers: [webhook]
            storage:
              path: "{db}"
            """
        )
    )
    try:
        # The script ends right after notify(); only the atexit flush
        # keeps the half-second delivery from being cut off.
        result = _run_isolated(
            """
            from opssignal import notify
            notify(source="pytest", event_type="check", title="exit")
            """,
            {"SIGNAL_CONFIG": str(config)},
        )
    finally:
        server.shutdown()
    assert result.returncode == 0, result.stderr
    rows = _run_isolated(
        """
        import sqlite3, sys
        print(sqlite3.connect(sys.argv[1]).execute(
            "SELECT delivery_status, provider FROM signals").fetchall())
        """,
        {},
        str(db),
    )
    assert rows.stdout.strip() == "[('delivered', 'webhook')]", rows.stderr
