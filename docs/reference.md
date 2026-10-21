# API reference

The Rust API of `opssignal-core` is generated with `cargo doc` and published
with this site:

**[Open the opssignal-core API reference](https://adithyapathipaka.github.io/OpsSignal/rust/opssignal_core/)**

The parts most callers need:

| Item | Purpose |
|---|---|
| `SignalClient::from_env()` | Build a client from the discovered config. |
| `SignalClient::from_config(&Config)` | Build a client from a config you loaded yourself. |
| `SignalClient::notify_async(SignalInput)` | Fire-and-forget delivery. The signal is saved as `pending` before this returns. |
| `SignalClient::flush(Duration)` | Wait for fire-and-forget deliveries to finish. Dropping the client does this for up to 5 seconds. |
| `SignalClient::notify_sync(SignalInput, Duration)` | Blocking delivery with a timeout. Returns a `DeliveryResult`, whose `deliveries` lists each provider's outcome. |
| `routing::DeliveryMode` | A route's `mode`: `Failover` (default) or `Fanout`. |
| `config::Config::{discover, from_file, from_yaml_str}` | Load `signal.yaml`. |
| `delivery::Provider` | Trait to implement for a new provider. |
| `sink::SignalSink` | Trait to implement for a new sink. |

The Python API is `opssignal.notify()` and `opssignal.flush(timeout=5.0)`,
plus the Airflow adapter; see [Getting started](getting-started.md). The
package flushes on interpreter exit by itself.

The C library exports `signal_notify_async(json)`, `signal_flush(timeout_ms)`
and `signal_free_string(ptr)`. Call `signal_flush` before the host exits, or
signals still being delivered are cut off (they stay recorded as `pending`).
