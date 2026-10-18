"""Cloud config sources come from add-on packages (pip extras)."""

import importlib.util
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from tests.test_delivery import _run_isolated

NOTIFY = """
from opssignal import notify
try:
    notify(source="pytest", event_type="check", title="t")
    print("ok")
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
    assert result.stdout.startswith("ok"), result.stdout
    # The sink path only exists in the S3 copy of the config.
    assert db.exists()
