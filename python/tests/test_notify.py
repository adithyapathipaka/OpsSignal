import pytest
from opssignal import notify


def test_valid_call_succeeds():
    notify(source="airflow", event_type="task_failed", title="A task failed")


def test_all_severity_values_accepted():
    for sev in ("info", "success", "warning", "error", "critical"):
        notify(source="ci", event_type="build_done", title="done", severity=sev)


def test_unknown_severity_defaults_to_info():
    # Unrecognised severity falls back to Info without raising.
    notify(source="ci", event_type="build_done", title="done", severity="bogus")


def test_optional_message_and_environment():
    notify(
        source="airflow",
        event_type="task_failed",
        title="Failure",
        message="Something went wrong",
        environment="prod",
    )


def test_empty_title_raises():
    with pytest.raises(ValueError, match="title must not be empty"):
        notify(source="airflow", event_type="task_failed", title="")


def test_whitespace_title_raises():
    with pytest.raises(ValueError, match="title must not be empty"):
        notify(source="airflow", event_type="task_failed", title="   ")


def test_empty_source_raises():
    with pytest.raises(ValueError, match="source must not be empty"):
        notify(source="", event_type="task_failed", title="A task failed")


def test_empty_event_type_raises():
    with pytest.raises(ValueError, match="event_type must not be empty"):
        notify(source="airflow", event_type="", title="A task failed")
