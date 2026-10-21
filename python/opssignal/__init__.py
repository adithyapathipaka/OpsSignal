"""opssignal — cross-language operational signal SDK.

Public API surface for v0.1:
    from opssignal import notify
    notify(source="airflow", event_type="task_failed", title="...", severity="warning")

notify() returns before delivery finishes. Signals still in flight when
the interpreter exits are flushed by an atexit hook (up to 5 seconds);
call flush() yourself to wait at a point of your choosing. flush() also
sends the digest of any group a grouped route is still holding.

The Airflow adapter is the flagship integration for v0.1 — see
opssignal.integrations.airflow.
"""

import atexit

from opssignal._native import flush, notify

atexit.register(flush)

__all__ = ["flush", "notify"]
__version__ = "0.1.0"
