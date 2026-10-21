"""Airflow integration.

Usage:
    from opssignal.integrations.airflow import failure_callback, success_callback

    default_args = {
        "on_failure_callback": failure_callback,
        # Resolves the failure alert once the task succeeds (added in
        # v1.0.0-beta.3).
        "on_success_callback": success_callback,
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

from opssignal import notify, resolve


def _safe_get(context: dict, key: str, default: Any = None) -> Any:
    """Airflow context dicts vary by version; never let a missing key
    raise inside an error-handling callback — that would replace a
    visible task failure with an invisible callback failure."""
    try:
        return context.get(key, default)
    except Exception:
        return default


def _ids(context: dict) -> tuple:
    dag_run = _safe_get(context, "dag_run")
    task_instance = _safe_get(context, "task_instance")
    dag_id = getattr(dag_run, "dag_id", None) or _safe_get(context, "dag", "unknown")
    task_id = getattr(task_instance, "task_id", "unknown")
    return dag_id, task_id, task_instance


def _failure_title(dag_id: Any, task_id: Any) -> str:
    # The dedup key is computed from source, event_type and title, so
    # success_callback resolves the alert by sending the same three.
    return f"Airflow task failed: {dag_id}.{task_id}"


def failure_callback(context: dict) -> None:
    """Drop-in on_failure_callback for Airflow tasks/DAGs.

    Extracts dag_id, task_id, and exception from the Airflow context
    and emits a `task_failed` signal at `error` severity.
    """
    dag_id, task_id, _ = _ids(context)
    exception = _safe_get(context, "exception")

    notify(
        source="airflow",
        event_type="task_failed",
        title=_failure_title(dag_id, task_id),
        severity="error",
        message=str(exception) if exception else None,
        environment=None,  # caller can wrap this adapter to inject env if needed
    )


def success_callback(context: dict) -> None:
    """Drop-in on_success_callback that resolves the task's failure alert.

    Sends a `resolve` with the dedup key failure_callback's alert got,
    so a later successful try (or run) closes it: PagerDuty resolves the
    incident, Opsgenie closes the alert, a Slack bot message is updated,
    and other providers that got the alert are told it is resolved.

    A success with no open alert recorded on this host sends nothing,
    except to PagerDuty and Opsgenie, which resolve by key themselves
    and ignore a key with nothing open. That is one Events or Alert API
    call per successful task for routes to them, which keeps working
    when the failure and the success run on different workers.
    """
    dag_id, task_id, task_instance = _ids(context)
    try_number = getattr(task_instance, "try_number", None)

    resolve(
        source="airflow",
        event_type="task_failed",
        title=_failure_title(dag_id, task_id),
        message=f"Succeeded on try {try_number}" if try_number else "Succeeded",
        environment=None,
    )
