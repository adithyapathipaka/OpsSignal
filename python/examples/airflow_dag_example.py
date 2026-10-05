"""Complete v0.1 MVP example: Airflow DAG with on_failure_callback wired
to opssignal.

Run `cp signal.example.yaml signal.yaml` and set SLACK_WEBHOOK_URL
before running this DAG, or just let it fall back to the console
provider with no configuration at all.
"""

from datetime import datetime

from airflow import DAG
from airflow.operators.python import PythonOperator

from opssignal.integrations.airflow import failure_callback


def run_dbt_models():
    raise RuntimeError("simulated dbt model failure for demo purposes")


with DAG(
    dag_id="dbt_core",
    start_date=datetime(2026, 1, 1),
    schedule=None,
    default_args={"on_failure_callback": failure_callback},
) as dag:
    run_models = PythonOperator(
        task_id="run_customers",
        python_callable=run_dbt_models,
    )
