from types import SimpleNamespace
from unittest.mock import patch

from opssignal.integrations.airflow import _safe_get, failure_callback


class TestSafeGet:
    def test_returns_value_for_present_key(self):
        assert _safe_get({"k": "v"}, "k") == "v"

    def test_returns_default_for_missing_key(self):
        assert _safe_get({}, "missing") is None
        assert _safe_get({}, "missing", "fallback") == "fallback"

    def test_returns_default_when_context_raises(self):
        class Broken:
            def get(self, key, default=None):
                raise RuntimeError("boom")

        assert _safe_get(Broken(), "k", "safe") == "safe"


class TestFailureCallback:
    def _make_context(self, dag_id="my_dag", task_id="my_task", run_id="run_1", exception=None):
        dag_run = SimpleNamespace(dag_id=dag_id, run_id=run_id)
        task_instance = SimpleNamespace(task_id=task_id, try_number=1)
        return {"dag_run": dag_run, "task_instance": task_instance, "exception": exception}

    def test_calls_notify_with_expected_args(self):
        ctx = self._make_context()
        with patch("opssignal.integrations.airflow.notify") as mock_notify:
            failure_callback(ctx)
        mock_notify.assert_called_once_with(
            source="airflow",
            event_type="task_failed",
            title="Airflow task failed: my_dag.my_task",
            severity="error",
            message=None,
            environment=None,
        )

    def test_includes_exception_message(self):
        ctx = self._make_context(exception=ValueError("bad value"))
        with patch("opssignal.integrations.airflow.notify") as mock_notify:
            failure_callback(ctx)
        _, kwargs = mock_notify.call_args
        assert kwargs["message"] == "bad value"

    def test_empty_context_does_not_raise(self):
        with patch("opssignal.integrations.airflow.notify"):
            failure_callback({})

    def test_none_dag_run_and_task_instance(self):
        ctx = {"dag_run": None, "task_instance": None}
        with patch("opssignal.integrations.airflow.notify") as mock_notify:
            failure_callback(ctx)
        _, kwargs = mock_notify.call_args
        assert "unknown" in kwargs["title"]
