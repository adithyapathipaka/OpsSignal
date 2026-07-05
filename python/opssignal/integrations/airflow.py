"""Airflow integration.

Usage:
    from opssignal.integrations.airflow import failure_callback

    default_args = {
        "on_failure_callback": failure_callback,
    }

Design note (see architecture review): this adapter deliberately uses
notify()'s default fire-and-forget behavior. A failure callback that
blocks on a flaky Slack webhook would add latency and a NEW failure
mode to the very DAG run it's supposed to be monitoring. If you need
a delivery guarantee for a specific callback, call opssignal.notify()
directly with explicit sync semantics instead of using this adapter.
"""

from __future__ import annotations

from typing import Any

from opssignal import notify


def _safe_get(context: dict, key: str, default: Any = None) -> Any:
    """Airflow context dicts vary by version; never let a missing key
    raise inside an error-handling callback — that would replace a
    visible task failure with an invisible callback failure."""
    try:
        return context.get(key, default)
    except Exception:
        return default


def failure_callback(context: dict) -> None:
    """Drop-in on_failure_callback for Airflow tasks/DAGs.

    Extracts dag_id, task_id, and exception from the Airflow context
    and emits a `task_failed` signal at `error` severity.
    """
    dag_run = _safe_get(context, "dag_run")
    task_instance = _safe_get(context, "task_instance")

    dag_id = getattr(dag_run, "dag_id", None) or _safe_get(context, "dag", "unknown")
    task_id = getattr(task_instance, "task_id", "unknown")
    exception = _safe_get(context, "exception")

    notify(
        source="airflow",
        event_type="task_failed",
        title=f"Airflow task failed: {dag_id}.{task_id}",
        severity="error",
        message=str(exception) if exception else None,
        environment=None,  # caller can wrap this adapter to inject env if needed
    )
