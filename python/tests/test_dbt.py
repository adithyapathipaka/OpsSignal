import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from opssignal.integrations import dbt
from opssignal.integrations.dbt import (
    callback,
    main,
    make_callback,
    report_run_results,
    signals_from_run_results,
)

FIXTURE = Path(__file__).parent / "fixtures" / "dbt_run_results.json"


def load_fixture():
    return json.loads(FIXTURE.read_text())


class TestSignalsFromRunResults:
    def test_reports_failures_and_warnings_only(self):
        signals = signals_from_run_results(load_fixture(), environment="prod")
        assert [s["event_type"] for s in signals] == [
            "dbt_model_failed",
            "dbt_test_failed",
            "dbt_test_warned",
        ]

    def test_failed_model_signal(self):
        model = signals_from_run_results(load_fixture(), environment="prod")[0]
        assert model["source"] == "dbt"
        assert model["title"] == "dbt model failed: orders"
        assert model["severity"] == "error"
        assert "column" in model["message"]
        assert model["environment"] == "prod"
        assert model["service"] == "jaffle_shop"
        assert model["component"] == "model.jaffle_shop.orders"
        assert model["run_id"] == "6f2c5f0e-3b7a-4c1e-9d55-0a1b2c3d4e5f"
        meta = model["metadata"]
        assert meta["status"] == "error"
        assert meta["resource_type"] == "model"
        assert meta["dbt_version"] == "1.9.4"
        assert meta["command"] == "build"
        assert meta["target"] == "prod"
        assert "failures" not in meta

    def test_test_signals_use_the_test_name_and_severity(self):
        _, failed, warned = signals_from_run_results(load_fixture())
        assert failed["title"] == "dbt test failed: not_null_stg_orders_order_id"
        assert failed["metadata"]["failures"] == 3
        assert warned["severity"] == "warning"
        assert warned["title"] == "dbt test warned: accepted_values_stg_orders_status"

    def test_legacy_runtime_error_status_is_a_failure(self):
        results = {"results": [{"status": "runtime error", "unique_id": "seed.p.countries"}]}
        (signal,) = signals_from_run_results(results)
        assert signal["event_type"] == "dbt_seed_failed"
        assert signal["severity"] == "error"

    def test_clean_run_sends_nothing(self):
        results = {"results": [{"status": "success", "unique_id": "model.p.a"}]}
        assert signals_from_run_results(results) == []

    def test_malformed_results_do_not_raise(self):
        assert signals_from_run_results({}) == []
        assert signals_from_run_results({"results": None}) == []
        (signal,) = signals_from_run_results({"results": ["junk", {"status": "error"}]})
        assert signal["component"] == "unknown"

    def test_many_failures_are_rolled_into_a_summary(self):
        results = {
            "metadata": {"invocation_id": "inv"},
            "results": [{"status": "fail", "unique_id": f"test.p.t{i}.abc123"} for i in range(30)],
        }
        signals = signals_from_run_results(results, max_signals=5)
        assert len(signals) == 5
        summary = signals[-1]
        assert summary["event_type"] == "dbt_run_degraded"
        assert summary["title"] == "dbt run had 26 more failing or warning nodes"
        assert summary["severity"] == "error"
        assert summary["metadata"]["failed"] == 26
        assert len(summary["metadata"]["unique_ids"]) == 26
        assert summary["run_id"] == "inv"

    def test_zero_max_signals_means_no_limit(self):
        results = {
            "results": [{"status": "fail", "unique_id": f"test.p.t{i}.x"} for i in range(30)]
        }
        assert len(signals_from_run_results(results, max_signals=0)) == 30


class TestReportRunResults:
    def test_sends_each_signal(self):
        with patch.object(dbt, "notify") as mock_notify:
            sent = report_run_results(FIXTURE, environment="prod")
        assert sent == 3
        assert mock_notify.call_count == 3
        kwargs = mock_notify.call_args_list[0].kwargs
        assert kwargs["title"] == "dbt model failed: orders"
        assert kwargs["environment"] == "prod"

    def test_missing_file_sends_nothing(self, tmp_path):
        with patch.object(dbt, "notify") as mock_notify:
            assert report_run_results(tmp_path / "nope.json") == 0
        mock_notify.assert_not_called()

    def test_invalid_json_sends_nothing(self, tmp_path):
        bad = tmp_path / "run_results.json"
        bad.write_text("{not json")
        with patch.object(dbt, "notify") as mock_notify:
            assert report_run_results(bad) == 0
        mock_notify.assert_not_called()

    def test_notify_error_does_not_stop_the_rest(self):
        with patch.object(dbt, "notify", side_effect=[RuntimeError("boom"), None, None]):
            assert report_run_results(FIXTURE) == 2

    def test_sends_through_the_real_binding(self):
        # End to end through the native module, against the test config.
        assert report_run_results(FIXTURE, environment="ci") == 3


class TestCallback:
    def _event(self, status, unique_id="model.jaffle_shop.orders", name="NodeFinished"):
        return SimpleNamespace(
            info=SimpleNamespace(name=name, invocation_id="inv-1"),
            data=SimpleNamespace(
                node_info=SimpleNamespace(
                    unique_id=unique_id, resource_type="model", materialized="table"
                ),
                run_result=SimpleNamespace(
                    status=status, message="boom", num_failures=0, execution_time=0.5
                ),
            ),
        )

    def test_failed_node_sends_a_signal(self):
        with patch.object(dbt, "notify") as mock_notify:
            callback(self._event("error"))
        kwargs = mock_notify.call_args.kwargs
        assert kwargs["title"] == "dbt model failed: orders"
        assert kwargs["run_id"] == "inv-1"
        assert kwargs["message"] == "boom"
        assert kwargs["metadata"]["materialized"] == "table"
        assert kwargs["environment"] is None

    def test_successful_node_and_other_events_are_ignored(self):
        with patch.object(dbt, "notify") as mock_notify:
            callback(self._event("success"))
            callback(self._event("error", name="NodeStart"))
            callback(SimpleNamespace())
        mock_notify.assert_not_called()

    def test_make_callback_sets_the_environment(self):
        with patch.object(dbt, "notify") as mock_notify:
            make_callback("prod")(self._event("fail", unique_id="test.p.t.abc"))
        assert mock_notify.call_args.kwargs["environment"] == "prod"
        assert mock_notify.call_args.kwargs["event_type"] == "dbt_model_failed"

    def test_callback_never_raises(self):
        with patch.object(dbt, "notify", side_effect=RuntimeError("down")):
            callback(self._event("error"))


class TestMain:
    def test_cli_reports_and_always_exits_zero(self, tmp_path):
        with patch.object(dbt, "notify") as mock_notify:
            assert main([str(FIXTURE), "--environment", "prod", "--max-signals", "2"]) == 0
            assert mock_notify.call_count == 2
            assert main([str(tmp_path / "missing.json")]) == 0
