"""Cloud config sources come from add-on packages (pip extras)."""

import importlib.util
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import ClassVar

import pytest

from tests.test_delivery import _run_isolated

NOTIFY = """
from opssignal import notify
try:
    notify(source="pytest", event_type="check", title="t")
    print("NOTIFY-RETURNED")
except RuntimeError as e:
    print("RuntimeError:", e)
"""


def _installed(module: str) -> bool:
    return importlib.util.find_spec(module) is not None


@pytest.mark.parametrize(
    ("url", "module", "extra"),
    [
        ("s3://ops-config/signal.yaml", "opssignal_aws", "aws"),
        ("gs://ops-config/signal.yaml", "opssignal_gcp", "gcp"),
        ("az://ops-config/signal.yaml", "opssignal_azure", "azure"),
    ],
)
def test_cloud_url_without_its_addon_names_the_extra(tmp_path, url, module, extra):
    if _installed(module):
        pytest.skip(f"{module} is installed")
    result = _run_isolated(NOTIFY, {"SIGNAL_CONFIG": url})

    assert result.returncode == 0, result.stderr
    assert f"opssignal[{extra}]" in result.stdout


class _ObjectStore(BaseHTTPRequestHandler):
    """Serves one object, path-style, the way S3 does for GET."""

    body = b""

    def do_GET(self):
        if self.path != "/ops-config/prod/signal.yaml":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(self.body)))
        self.send_header("Last-Modified", "Sun, 18 Oct 2026 12:00:00 GMT")
        self.send_header("ETag", '"1"')
        self.end_headers()
        self.wfile.write(self.body)

    def log_message(self, *args):
        pass


@pytest.mark.skipif(not _installed("opssignal_aws"), reason="opssignal-aws not installed")
def test_s3_config_loads_through_the_aws_addon(tmp_path):
    db = tmp_path / "from-s3.db"
    _ObjectStore.body = f'storage:\n  path: "{db}"\n'.encode()
    server = ThreadingHTTPServer(("127.0.0.1", 0), _ObjectStore)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        result = _run_isolated(
            NOTIFY,
            {
                "SIGNAL_CONFIG": "s3://ops-config/prod/signal.yaml",
                "SIGNAL_CONFIG_CACHE_DIR": str(tmp_path / "cache"),
                "AWS_ENDPOINT_URL": f"http://127.0.0.1:{server.server_port}",
                "AWS_ALLOW_HTTP": "true",
                "AWS_ACCESS_KEY_ID": "test",
                "AWS_SECRET_ACCESS_KEY": "test",
                "AWS_REGION": "us-east-1",
            },
        )
    finally:
        server.shutdown()

    assert result.returncode == 0, result.stderr
    assert "NOTIFY-RETURNED" in result.stdout, result.stdout
    # The sink path only exists in the S3 copy of the config.
    assert db.exists()


def test_secret_without_its_addon_names_the_extra(tmp_path):
    if _installed("opssignal_aws"):
        pytest.skip("opssignal_aws is installed")
    config = tmp_path / "signal.yaml"
    config.write_text('providers:\n  slack:\n    webhook_url: "${aws-sm:prod/slack}"\n')
    result = _run_isolated(NOTIFY, {"SIGNAL_CONFIG": str(config)})

    assert result.returncode == 0, result.stderr
    assert "opssignal[aws]" in result.stdout, result.stdout


class _SecretsManager(BaseHTTPRequestHandler):
    """Answers GetSecretValue for one secret."""

    value = ""

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        target = self.headers.get("x-amz-target")
        if target != "secretsmanager.GetSecretValue" or b'"prod/sink"' not in body:
            self.send_error(400)
            return
        payload = json.dumps({"Name": "prod/sink", "SecretString": self.value}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


@pytest.mark.skipif(not _installed("opssignal_aws"), reason="opssignal-aws not installed")
def test_secret_resolves_through_the_aws_addon(tmp_path):
    db = tmp_path / "from-secret.db"
    _SecretsManager.value = str(db)
    config = tmp_path / "signal.yaml"
    config.write_text('storage:\n  path: "${aws-sm:prod/sink}"\n')
    server = ThreadingHTTPServer(("127.0.0.1", 0), _SecretsManager)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        result = _run_isolated(
            NOTIFY,
            {
                "SIGNAL_CONFIG": str(config),
                "AWS_ENDPOINT_URL_SECRETSMANAGER": f"http://127.0.0.1:{server.server_port}",
                "AWS_ACCESS_KEY_ID": "test",
                "AWS_SECRET_ACCESS_KEY": "test",
                "AWS_REGION": "us-east-1",
            },
        )
    finally:
        server.shutdown()

    assert result.returncode == 0, result.stderr
    assert "NOTIFY-RETURNED" in result.stdout, result.stdout
    # The sink path only exists in the secret.
    assert db.exists()


SES_CONFIG = """\
providers:
  email:
    from: "OpsSignal <alerts@example.test>"
    to: [oncall@example.test]
    ses:
      region: us-east-1
routes:
  - match:
      severity: [error]
    providers: [email]
storage:
  path: "{db}"
"""

NOTIFY_ERROR = """
from opssignal import flush, notify
try:
    notify(source="pytest", event_type="check", title="ses check", severity="error")
    print("FLUSHED", flush(10))
except RuntimeError as e:
    print("RuntimeError:", e)
"""


def test_ses_email_without_the_aws_addon_names_the_extra(tmp_path):
    if _installed("opssignal_aws"):
        pytest.skip("opssignal_aws is installed")
    config = tmp_path / "signal.yaml"
    config.write_text(SES_CONFIG.format(db=tmp_path / "s.db"))
    result = _run_isolated(NOTIFY_ERROR, {"SIGNAL_CONFIG": str(config)})

    assert result.returncode == 0, result.stderr
    assert "opssignal[aws]" in result.stdout, result.stdout


class _Ses(BaseHTTPRequestHandler):
    """Accepts SES v2 SendEmail and keeps the requests."""

    received: ClassVar[list] = []

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path != "/v2/email/outbound-emails":
            self.send_error(404)
            return
        self.received.append((self.headers.get("authorization", ""), json.loads(body)))
        payload = b'{"MessageId": "m1"}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


@pytest.mark.skipif(not _installed("opssignal_aws"), reason="opssignal-aws not installed")
def test_ses_email_is_sent_through_the_aws_addon(tmp_path):
    import base64

    config = tmp_path / "signal.yaml"
    config.write_text(SES_CONFIG.format(db=tmp_path / "s.db"))
    _Ses.received = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Ses)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        result = _run_isolated(
            NOTIFY_ERROR,
            {
                "SIGNAL_CONFIG": str(config),
                "AWS_ENDPOINT_URL_SESV2": f"http://127.0.0.1:{server.server_port}",
                "AWS_ACCESS_KEY_ID": "test",
                "AWS_SECRET_ACCESS_KEY": "test",
                "AWS_REGION": "us-east-1",
            },
        )
    finally:
        server.shutdown()

    assert result.returncode == 0, result.stderr
    assert "FLUSHED True" in result.stdout, result.stdout + result.stderr
    assert len(_Ses.received) == 1
    authorization, body = _Ses.received[0]
    assert "/us-east-1/ses/aws4_request" in authorization
    raw = base64.b64decode(body["Content"]["Raw"]["Data"]).decode()
    assert "Subject: [ERROR] ses check" in raw
