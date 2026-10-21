"""dbt integration.

Two ways to hook dbt up, depending on how it runs:

1. After any dbt command, from a shell, CI job or orchestrator task. dbt
   writes target/run_results.json on every run, so this works with dbt
   Core, dbt Fusion and artifacts downloaded from dbt Cloud:

       dbt build; status=$?
       python -m opssignal.integrations.dbt target/run_results.json --environment prod
       exit $status

   or from Python:

       from opssignal.integrations.dbt import report_run_results
       report_run_results("target/run_results.json", environment="prod")

2. In-process, when dbt is invoked programmatically (dbt-core 1.5+):

       from dbt.cli.main import dbtRunner
       from opssignal.integrations.dbt import callback

       dbtRunner(callbacks=[callback]).invoke(["build"])

   The callback fires as each node finishes, so an alert goes out while
   the rest of the run is still going.

Both emit one signal per failed node (`error`) or warned test
(`warning`). To get one message per run instead, group the route by run
(the invocation id is the signal's `run_id`):

    routes:
      - match: {source: dbt}
        providers: [slack]
        group_by: [run_id]
        group_wait: 2m

and pass `--max-signals 0` (or `max_signals=0`), since the digest already
keeps a large run to one message. The digest goes out when `group_wait`
runs out or the process exits, whichever comes first. Skipped nodes are not reported: they are skipped because an
upstream node failed, and that node already has its own signal. Like the
Airflow adapter, nothing here ever raises into dbt or the caller's run,
and notify() is fire-and-forget.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
from typing import Any

from opssignal import flush, notify

logger = logging.getLogger(__name__)

SOURCE = "dbt"

# run_results.json and NodeFinished statuses. Older dbt versions write
# "runtime error" for what newer ones call "error".
_FAILED = {"error", "fail", "runtime error"}
_WARNED = {"warn"}

# One broken source can fail hundreds of tests at once. Past this many,
# the rest are rolled into a single summary signal.
DEFAULT_MAX_SIGNALS = 20


def _resource_type(unique_id: str, fallback: str | None = None) -> str:
    if fallback:
        return str(fallback)
    return unique_id.split(".", 1)[0] if "." in unique_id else "node"


def _node_name(unique_id: str) -> str:
    parts = unique_id.split(".")
    # model.<project>.<name>; tests carry a hash suffix after the name.
    if parts[0] in ("test", "unit_test") and len(parts) >= 4:
        return parts[2]
    return parts[-1] if parts else unique_id


def _project(unique_id: str) -> str | None:
    parts = unique_id.split(".")
    return parts[1] if len(parts) >= 3 else None


def _node_signal(
    *,
    unique_id: str,
    status: str,
    message: str | None,
    resource_type: str | None,
    invocation_id: str | None,
    environment: str | None,
    extra: dict[str, Any],
) -> dict[str, Any] | None:
    status = (status or "").lower()
    if status in _FAILED:
        severity, outcome = "error", "failed"
    elif status in _WARNED:
        severity, outcome = "warning", "warned"
    else:
        return None

    kind = _resource_type(unique_id, resource_type)
    name = _node_name(unique_id)
    metadata = {"unique_id": unique_id, "resource_type": kind, "status": status}
    metadata.update({k: v for k, v in extra.items() if v is not None})
    return {
        "source": SOURCE,
        "event_type": f"dbt_{kind}_{outcome}",
        "title": f"dbt {kind.replace('_', ' ')} {outcome}: {name}",
        "severity": severity,
        "message": message or None,
        "environment": environment,
        "service": _project(unique_id),
        "component": unique_id,
        "run_id": invocation_id,
        "metadata": metadata,
    }


def signals_from_run_results(
    run_results: dict[str, Any],
    *,
    environment: str | None = None,
    max_signals: int = DEFAULT_MAX_SIGNALS,
) -> list[dict[str, Any]]:
    """The notify() keyword arguments for a parsed run_results.json, in
    file order. Exposed so callers can inspect or adjust signals before
    sending them."""
    meta = run_results.get("metadata") or {}
    args = run_results.get("args") or {}
    invocation_id = meta.get("invocation_id")
    common = {
        "dbt_version": meta.get("dbt_version"),
        "command": args.get("which"),
        "target": args.get("target"),
    }

    signals = []
    for result in run_results.get("results") or []:
        if not isinstance(result, dict):
            continue
        signal = _node_signal(
            unique_id=str(result.get("unique_id") or "unknown"),
            status=str(result.get("status") or ""),
            message=result.get("message"),
            resource_type=None,
            invocation_id=invocation_id,
            environment=environment,
            extra={
                **common,
                "failures": result.get("failures"),
                "execution_time": result.get("execution_time"),
                "relation_name": result.get("relation_name"),
            },
        )
        if signal:
            signals.append(signal)

    if max_signals > 0 and len(signals) > max_signals:
        kept, rest = signals[: max_signals - 1], signals[max_signals - 1 :]
        failed = sum(1 for s in rest if s["severity"] == "error")
        signals = [
            *kept,
            {
                "source": SOURCE,
                "event_type": "dbt_run_degraded",
                "title": f"dbt run had {len(rest)} more failing or warning nodes",
                "severity": "error" if failed else "warning",
                "message": "\n".join(s["component"] for s in rest),
                "environment": environment,
                "service": kept[0]["service"] if kept else None,
                "run_id": invocation_id,
                "metadata": {
                    **{k: v for k, v in common.items() if v is not None},
                    "failed": failed,
                    "warned": len(rest) - failed,
                    "unique_ids": [s["component"] for s in rest],
                },
            },
        ]
    return signals


def report_run_results(
    path: str | os.PathLike = "target/run_results.json",
    *,
    environment: str | None = None,
    max_signals: int = DEFAULT_MAX_SIGNALS,
) -> int:
    """Reads a dbt run_results.json and sends a signal for each failed
    or warned node. Returns how many signals were sent. A missing or
    unreadable file is logged, not raised, and sends nothing."""
    try:
        with open(path, encoding="utf-8") as f:
            run_results = json.load(f)
    except (OSError, ValueError) as e:
        logger.warning("opssignal: cannot read dbt run results %s: %s", path, e)
        return 0
    if not isinstance(run_results, dict):
        logger.warning("opssignal: %s is not a dbt run_results.json", path)
        return 0

    sent = 0
    for signal in signals_from_run_results(
        run_results, environment=environment, max_signals=max_signals
    ):
        try:
            notify(**signal)
            sent += 1
        except Exception as e:
            logger.warning("opssignal: could not send dbt signal: %s", e)
    return sent


def _get(obj: Any, *path: str) -> Any:
    for name in path:
        if obj is None:
            return None
        obj = obj.get(name) if isinstance(obj, dict) else getattr(obj, name, None)
    return obj


def make_callback(environment: str | None = None):
    """A dbtRunner callback that tags its signals with `environment`."""

    def _callback(event: Any) -> None:
        try:
            if _get(event, "info", "name") != "NodeFinished":
                return
            node = _get(event, "data", "node_info")
            result = _get(event, "data", "run_result")
            unique_id = str(_get(node, "unique_id") or "unknown")
            signal = _node_signal(
                unique_id=unique_id,
                status=str(_get(result, "status") or ""),
                message=_get(result, "message"),
                resource_type=_get(node, "resource_type"),
                invocation_id=_get(event, "info", "invocation_id"),
                environment=environment,
                extra={
                    "failures": _get(result, "num_failures"),
                    "execution_time": _get(result, "execution_time"),
                    "materialized": _get(node, "materialized") or None,
                },
            )
            if signal:
                notify(**signal)
        except Exception as e:
            # Never let alerting break the dbt run it is watching.
            logger.warning("opssignal: dbt callback failed: %s", e)

    return _callback


callback = make_callback()
"""Drop-in dbtRunner callback, without an environment."""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m opssignal.integrations.dbt",
        description="Send an OpsSignal signal for each failed or warned node in a dbt run.",
    )
    parser.add_argument(
        "run_results",
        nargs="?",
        default="target/run_results.json",
        help="path to run_results.json (default: target/run_results.json)",
    )
    parser.add_argument("--environment", help="environment to tag signals with, such as prod")
    parser.add_argument(
        "--max-signals",
        type=int,
        default=DEFAULT_MAX_SIGNALS,
        help=(
            "roll nodes past this many into one summary signal "
            f"(default {DEFAULT_MAX_SIGNALS}; 0 for no limit)"
        ),
    )
    args = parser.parse_args(argv)
    sent = report_run_results(
        args.run_results, environment=args.environment, max_signals=args.max_signals
    )
    flush()
    sys.stderr.write(f"opssignal: sent {sent} dbt signal(s)\n")
    # Always 0: the dbt command's own exit status is what should fail the job.
    return 0


if __name__ == "__main__":
    sys.exit(main())
