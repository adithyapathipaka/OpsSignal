"""send(): the blocking call that returns how delivery went."""

import http.server
import json
import os
import subprocess  # runs this interpreter only
import sys
import textwrap
import threading

import pytest
from opssignal import DeliveryResult, ProviderResult, send


def test_send_returns_the_delivery_result():
    result = send(source="pytest", event_type="check", title="send check", severity="warning")

    assert isinstance(result, DeliveryResult)
    assert result.status == "delivered"
    assert result.delivered
    assert result.provider == "console"
    assert result.error is None
    assert result.suppressed == []
    assert result.grouped == []
    assert len(result.signal_id) == 36
    assert result.dedup_key
    [console] = result.deliveries
    assert isinstance(console, ProviderResult)
    assert (console.provider, console.status, console.attempts) == ("console", "delivered", 1)


def test_send_keeps_the_callers_dedup_key():
    result = send(source="pytest", event_type="check", title="t", dedup_key="incident-42")
    assert result.dedup_key == "incident-42"


def test_send_resolves_an_alert_and_returns_its_result():
    raised = send(source="pytest", event_type="check", title="resolve me")
    result = send("", "", "", dedup_key=raised.dedup_key, action="resolve")

    assert isinstance(result, DeliveryResult)
    assert result.dedup_key == raised.dedup_key
    assert result.signal_id != raised.signal_id


def test_send_rejects_an_unknown_action():
    with pytest.raises(ValueError, match="invalid signal"):
        send(source="pytest", event_type="check", title="t", action="close")


def test_to_dict_is_json_serialisable():
    d = send(source="pytest", event_type="check", title="t").to_dict()

    assert json.loads(json.dumps(d)) == d
    assert d["status"] == "delivered"
    assert d["deliveries"] == [
        {"provider": "console", "status": "delivered", "error": None, "attempts": 1}
    ]


def test_send_rejects_invalid_input_and_timeout():
    with pytest.raises(ValueError, match="source must not be empty"):
        send(source="", event_type="check", title="t")
    with pytest.raises(ValueError, match="invalid timeout"):
        send(source="pytest", event_type="check", title="t", timeout=-1)


class _FailingHook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        self.send_response(500)
        self.end_headers()

    def log_message(self, *args):
        pass


def test_fanout_with_one_provider_down_is_partial(tmp_path):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _FailingHook)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = tmp_path / "signal.yaml"
    config.write_text(
        textwrap.dedent(
            f"""\
            providers:
              webhook:
                url: "http://127.0.0.1:{server.server_port}/hook"
            routes:
              - match: {{}}
                mode: fanout
                providers: [console, webhook]
            storage:
              path: "{tmp_path / "signals.db"}"
            """
        )
    )
    try:
        # A fresh interpreter, because the client is built once per process.
        result = subprocess.run(  # noqa: S603 - fixed argv, no shell
            [
                sys.executable,
                "-c",
                textwrap.dedent(
                    """
                    import json
                    from opssignal import send
                    r = send(source="pytest", event_type="check", title="t", timeout=20)
                    print(json.dumps(r.to_dict()))
                    """
                ),
            ],
            env={**os.environ, "SIGNAL_CONFIG": str(config)},
            capture_output=True,
            text=True,
            timeout=60,
        )
    finally:
        server.shutdown()

    assert result.returncode == 0, result.stderr
    report = json.loads(result.stdout.strip().splitlines()[-1])
    assert report["status"] == "partial"
    assert report["provider"] == "console"
    outcomes = {d["provider"]: d for d in report["deliveries"]}
    assert outcomes["console"]["status"] == "delivered"
    assert outcomes["webhook"]["status"] == "failed"
    assert outcomes["webhook"]["attempts"] == 3
    assert "webhook" in report["error"]


def test_signal_held_for_a_digest_is_grouped(tmp_path):
    config = tmp_path / "signal.yaml"
    config.write_text(
        textwrap.dedent(
            f"""\
            routes:
              - match: {{}}
                providers: [console]
                group_by: [run_id]
                group_wait: 10m
            storage:
              path: "{tmp_path / "signals.db"}"
            """
        )
    )
    result = subprocess.run(  # noqa: S603 - fixed argv, no shell
        [
            sys.executable,
            "-c",
            textwrap.dedent(
                """
                import json
                from opssignal import send
                r = send(source="pytest", event_type="check", title="t", run_id="r1")
                print(json.dumps(r.to_dict()))
                """
            ),
        ],
        env={**os.environ, "SIGNAL_CONFIG": str(config)},
        capture_output=True,
        text=True,
        timeout=60,
    )

    assert result.returncode == 0, result.stderr
    # The digest itself goes to the console when the interpreter exits.
    [line] = [x for x in result.stdout.splitlines() if x.startswith("{")]
    report = json.loads(line)
    assert report["status"] == "grouped"
    assert len(report["grouped"]) == 1
