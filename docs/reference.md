# API reference

The Rust API of `opssignal-core` is generated with `cargo doc` and published
with this site:

<a href="../rust/opssignal_core/"><strong>Open the opssignal-core API reference</strong></a>

The reference matches the docs version picked at the top of the page.

The parts most callers need:

| Item | Purpose |
|---|---|
| `SignalClient::from_env()` | Build a client from the discovered config. |
| `SignalClient::from_config(&Config)` | Build a client from a config you loaded yourself. |
| `SignalClient::notify_async(SignalInput)` | Fire-and-forget delivery. The signal is saved as `pending` before this returns. |
| `SignalClient::flush(Duration)` | Wait for fire-and-forget deliveries to finish. Dropping the client does this for up to 5 seconds. |
| `SignalClient::notify_sync(SignalInput, Duration)` | Blocking delivery with a timeout. Returns a `DeliveryResult`, whose `deliveries` lists each provider's outcome. |
| `signal::Action` | A signal's `action`: `Trigger` (default), `Acknowledge` or `Resolve`. Added in v1.0.0-beta.3. |
| `routing::DeliveryMode` | A route's `mode`: `Failover` (default) or `Fanout`. |
| `config::Config::{discover, from_file, from_yaml_str}` | Load `signal.yaml`. |
| `delivery::Provider` | Trait to implement for a new provider. |
| `sink::SignalSink` | Trait to implement for a new sink. |

The Python API is `opssignal.notify()` and `opssignal.flush(timeout=5.0)`,
plus the Airflow adapter; see [Getting started](getting-started.md).
`opssignal.acknowledge()` and `opssignal.resolve()` (added in v1.0.0-beta.3)
act on an alert raised earlier; see [Acknowledge and resolve](lifecycle.md). The
package flushes on interpreter exit by itself.

The C library exports `signal_notify_async(json)`, `signal_flush(timeout_ms)`
and `signal_free_string(ptr)`. The JSON may set `"action"` to `"acknowledge"`
or `"resolve"` (added in v1.0.0-beta.3), with just a `dedup_key`. Call `signal_flush` before the host exits, or
signals still being delivered are cut off (they stay recorded as `pending`).
