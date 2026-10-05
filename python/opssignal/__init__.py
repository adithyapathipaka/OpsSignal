"""opssignal — cross-language operational signal SDK.

Public API surface for v0.1:
    from opssignal import notify
    notify(source="airflow", event_type="task_failed", title="...", severity="warning")

The Airflow adapter is the flagship integration for v0.1 — see
opssignal.integrations.airflow.
"""

from opssignal._native import notify

__all__ = ["notify"]
__version__ = "0.1.0"
