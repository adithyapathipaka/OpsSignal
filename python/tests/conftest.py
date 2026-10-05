import pytest


@pytest.fixture(autouse=True, scope="session")
def isolated_signal_config(tmp_path_factory):
    """Point the SDK at a throwaway config so test runs never read a
    developer's ./signal.yaml or write signals.db into the repo."""
    tmp = tmp_path_factory.mktemp("opssignal")
    config = tmp / "signal.yaml"
    config.write_text(f'storage:\n  path: "{tmp / "signals.db"}"\n')
    mp = pytest.MonkeyPatch()
    mp.setenv("SIGNAL_CONFIG", str(config))
    yield
    mp.undo()
