"""opssignal — cross-language operational signal SDK.

Public API surface for v0.1:
    from opssignal import notify
    notify(source="airflow", event_type="task_failed", title="...", severity="warning")

notify() returns before delivery finishes. Signals still in flight when
the interpreter exits are flushed by an atexit hook (up to 5 seconds);
call flush() yourself to wait at a point of your choosing. flush() also
sends the digest of any group a grouped route is still holding.

acknowledge() and resolve() act on an alert raised earlier with the same
dedup key: PagerDuty and Opsgenie acknowledge or close it, a Slack bot
message is updated in place, and other providers that got the alert are
told it is acknowledged or resolved. Added in v1.0.0-beta.3.

The Airflow adapter is the flagship integration for v0.1 — see
opssignal.integrations.airflow.
"""

import atexit
from typing import Any, Optional

from opssignal._native import flush, notify

atexit.register(flush)


def resolve(
    dedup_key: Optional[str] = None,
    *,
    source: str = "",
    event_type: str = "",
    title: str = "",
    message: Optional[str] = None,
    environment: Optional[str] = None,
    **fields: Any,
) -> None:
    """Resolves the alert with ``dedup_key``, fire-and-forget like notify().

    Without ``dedup_key``, pass the source, event_type, title and
    environment the alert was raised with, so the same key is computed.
    Other keyword arguments (service, team, metadata, ...) are passed to
    notify().
    """
    notify(
        source,
        event_type,
        title,
        message=message,
        environment=environment,
        dedup_key=dedup_key,
        action="resolve",
        **fields,
    )


def acknowledge(
    dedup_key: Optional[str] = None,
    *,
    source: str = "",
    event_type: str = "",
    title: str = "",
    message: Optional[str] = None,
    environment: Optional[str] = None,
    **fields: Any,
) -> None:
    """Acknowledges the alert with ``dedup_key``; see resolve()."""
    notify(
        source,
        event_type,
        title,
        message=message,
        environment=environment,
        dedup_key=dedup_key,
        action="acknowledge",
        **fields,
    )


__all__ = ["acknowledge", "flush", "notify", "resolve"]
__version__ = "0.1.0"
