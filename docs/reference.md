# API reference

The Rust API of `opssignal-core` is generated with `cargo doc` and published
with this site:

**[Open the opssignal-core API reference](https://adithyapathipaka.github.io/OpsSignal/rust/opssignal_core/)**

The parts most callers need:

| Item | Purpose |
|---|---|
| `SignalClient::from_env()` | Build a client from the discovered config. |
| `SignalClient::from_config(&Config)` | Build a client from a config you loaded yourself. |
| `SignalClient::notify_async(SignalInput)` | Fire-and-forget delivery. |
| `SignalClient::notify_sync(SignalInput, Duration)` | Blocking delivery with a timeout. Returns a `DeliveryResult`. |
| `config::Config::{discover, from_file, from_yaml_str}` | Load `signal.yaml`. |
| `delivery::Provider` | Trait to implement for a new provider. |
| `sink::SignalSink` | Trait to implement for a new sink. |

The Python API is a single function, `opssignal.notify()`, plus the Airflow
adapter; see [Getting started](getting-started.md).
