//! CRITICAL RULE (see architecture review, section 7):
//!
//! Every single `#[no_mangle] extern "C"` entry point in this crate
//! MUST be wrapped in `catch_unwind`. No exceptions, including in
//! debug/CLI builds. A Rust panic crossing the FFI boundary is
//! undefined behavior in the host language — in Python that can mean
//! a hard segfault of the calling process. For an Airflow worker, that
//! takes down the very task the SDK was supposed to be monitoring.
//!
//! The macro below is the only sanctioned way to define an FFI entry
//! point in this crate. Do not write a raw `extern "C" fn` elsewhere.

use opssignal_core::SignalClient;
use std::ffi::{c_char, CStr, CString};
use std::panic;
use std::sync::OnceLock;
use std::time::Duration;

/// Process-global client, built from signal.yaml on first use. A config
/// error is cached and returned on every call.
static CLIENT: OnceLock<Result<SignalClient, String>> = OnceLock::new();

fn client() -> Result<&'static SignalClient, String> {
    CLIENT
        .get_or_init(|| SignalClient::from_env().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| format!("opssignal is not configured: {e}"))
}

/// Result struct returned across the FFI boundary. Both fields are
/// always present; `error` is null on success. Callers in each language
/// binding are responsible for freeing the returned pointer via
/// `signal_free_string`.
#[repr(C)]
pub struct CResult {
    pub success: bool,
    pub error: *mut c_char,
}

impl CResult {
    fn ok() -> Self {
        CResult {
            success: true,
            error: std::ptr::null_mut(),
        }
    }

    fn error(msg: impl Into<String>) -> Self {
        let c_string = CString::new(msg.into()).unwrap_or_else(|_| {
            CString::new("error message contained null byte")
                .expect("fallback literal has no null bytes")
        });
        CResult {
            success: false,
            error: c_string.into_raw(),
        }
    }
}

/// Every panic that escapes `f` is converted into a `CResult::error`
/// instead of unwinding across the FFI boundary. This is the single
/// chokepoint all entry points must go through.
fn guarded<F>(f: F) -> CResult
where
    F: FnOnce() -> Result<(), String> + panic::UnwindSafe,
{
    match panic::catch_unwind(f) {
        Ok(Ok(())) => CResult::ok(),
        Ok(Err(e)) => CResult::error(e),
        Err(_) => CResult::error("internal panic in opssignal-core — signal not sent"),
    }
}

/// # Safety
/// `json_ptr` must be null or a valid, null-terminated UTF-8 C string
/// owned by the caller for the duration of this call. A null pointer is
/// reported as an error rather than dereferenced.
#[no_mangle]
pub unsafe extern "C" fn signal_notify_async(json_ptr: *const c_char) -> CResult {
    guarded(|| {
        let input = unsafe { parse_input(json_ptr) }?;
        client()?.notify_async(input);
        Ok(())
    })
}

/// Blocking send: delivers the signal, waiting up to `timeout_ms`, and
/// writes how it went to `*result_json` as a JSON object:
///
/// ```json
/// {"signal_id": "...", "dedup_key": "...", "status": "partial",
///  "provider": "slack", "error": "pagerduty: ...", "attempts": 3,
///  "deliveries": [{"provider": "slack", "status": "delivered",
///                  "error": null, "attempts": 1}, ...],
///  "suppressed": [], "grouped": []}
/// ```
///
/// `status` is `delivered`, `partial`, `failed`, `skipped` or
/// `grouped` (held for a digest that `signal_flush` or the route's
/// `group_wait` sends). A failed
/// delivery is a successful call: `success` is false only when there is
/// no result (invalid input, no config, the signal could not be
/// recorded), and `*result_json` is then null. Free a non-null
/// `*result_json` with `signal_free_string`.
///
/// # Safety
/// `json_ptr` follows the rules of [`signal_notify_async`].
/// `result_json` must be null or point to writable storage for one
/// pointer; a null `result_json` is reported as an error.
#[no_mangle]
pub unsafe extern "C" fn signal_send(
    json_ptr: *const c_char,
    timeout_ms: u64,
    result_json: *mut *mut c_char,
) -> CResult {
    guarded(|| {
        if result_json.is_null() {
            return Err("result_json pointer is null".to_string());
        }
        unsafe { *result_json = std::ptr::null_mut() };
        let input = unsafe { parse_input(json_ptr) }?;
        let report = client()?
            .send(input, Duration::from_millis(timeout_ms))
            .map_err(|e| format!("signal not sent: {e}"))?;
        let json = serde_json::to_string(&report)
            .map_err(|e| format!("could not encode the delivery result: {e}"))?;
        let json = CString::new(json).map_err(|e| format!("delivery result: {e}"))?;
        unsafe { *result_json = json.into_raw() };
        Ok(())
    })
}

/// Reads and validates a signal JSON payload from C.
///
/// # Safety
/// `json_ptr` must be null or a valid, null-terminated C string.
unsafe fn parse_input(
    json_ptr: *const c_char,
) -> Result<opssignal_core::signal::SignalInput, String> {
    if json_ptr.is_null() {
        return Err("signal payload pointer is null".to_string());
    }
    let json_str = unsafe { CStr::from_ptr(json_ptr) }
        .to_str()
        .map_err(|e| format!("invalid UTF-8 in signal payload: {e}"))?;

    let input: opssignal_core::signal::SignalInput =
        serde_json::from_str(json_str).map_err(|e| format!("invalid signal JSON: {e}"))?;
    opssignal_core::signal::validate(&input).map_err(|e| e.to_string())?;
    Ok(input)
}

/// Sends the digests of any open groups (routes with `group_by` or
/// `group_wait`), then waits up to `timeout_ms` for signals sent with
/// `signal_notify_async` to finish delivering. Fails if some were still
/// in flight; those stay recorded as pending. The library has no exit hook of its own (the
/// process-global client is never dropped), so hosts should call this
/// before exiting.
#[no_mangle]
pub extern "C" fn signal_flush(timeout_ms: u64) -> CResult {
    guarded(|| match CLIENT.get() {
        Some(Ok(client)) if !client.flush(Duration::from_millis(timeout_ms)) => {
            Err("timed out with signals still being delivered".to_string())
        }
        _ => Ok(()),
    })
}

/// Frees a string previously returned in a `CResult.error` field or
/// through `signal_send`'s `result_json`.
/// Every language binding MUST call this after reading the error,
/// or the allocation leaks.
///
/// # Safety
/// `ptr` must have been returned by this library and not freed already.
#[no_mangle]
pub unsafe extern "C" fn signal_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(ptr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_converts_panic_to_error_result() {
        let result = guarded(|| -> Result<(), String> {
            panic!("simulated panic");
        });
        assert!(!result.success);
        assert!(!result.error.is_null());
    }

    #[test]
    fn guarded_passes_through_ok() {
        let result = guarded(|| Ok(()));
        assert!(result.success);
        assert!(result.error.is_null());
    }

    /// Reads and frees the error string, as a language binding would.
    fn take_error(result: CResult) -> String {
        assert!(!result.success);
        assert!(!result.error.is_null());
        let msg = unsafe { CStr::from_ptr(result.error) }
            .to_string_lossy()
            .into_owned();
        unsafe { signal_free_string(result.error) };
        msg
    }

    fn notify(bytes: &[u8]) -> CResult {
        let c = CString::new(bytes).expect("test payload has no interior null");
        unsafe { signal_notify_async(c.as_ptr()) }
    }

    #[test]
    fn guarded_panic_message_says_signal_not_sent() {
        let result = guarded(|| -> Result<(), String> { panic!("boom") });
        assert!(take_error(result).contains("signal not sent"));
    }

    #[test]
    fn guarded_error_is_returned_verbatim() {
        let result = guarded(|| Err("nope".to_string()));
        assert_eq!(take_error(result), "nope");
    }

    #[test]
    fn error_with_interior_null_byte_still_returns_a_message() {
        let msg = take_error(CResult::error("bad\0message"));
        assert_eq!(msg, "error message contained null byte");
    }

    #[test]
    fn null_payload_is_an_error_not_a_crash() {
        let result = unsafe { signal_notify_async(std::ptr::null()) };
        assert!(take_error(result).contains("null"));
    }

    #[test]
    fn invalid_utf8_payload_is_an_error() {
        assert!(take_error(notify(&[0xff, 0xfe, b'{'])).contains("invalid UTF-8"));
    }

    #[test]
    fn invalid_json_payload_is_an_error() {
        assert!(take_error(notify(b"{not json")).contains("invalid signal JSON"));
    }

    #[test]
    fn unknown_severity_in_json_is_an_error() {
        let payload = br#"{"source":"ci","event_type":"e","title":"t","severity":"bogus"}"#;
        assert!(take_error(notify(payload)).contains("invalid signal JSON"));
    }

    #[test]
    fn blank_required_field_is_a_validation_error() {
        let payload = br#"{"source":"ci","event_type":"e","title":" ","severity":"info"}"#;
        assert!(take_error(notify(payload)).contains("title must not be empty"));
    }

    fn send(bytes: &[u8], out: *mut *mut c_char) -> CResult {
        let c = CString::new(bytes).expect("test payload has no interior null");
        unsafe { signal_send(c.as_ptr(), 5_000, out) }
    }

    #[test]
    fn send_with_null_result_pointer_is_an_error_not_a_crash() {
        let payload = br#"{"source":"ci","event_type":"e","title":"t","severity":"info"}"#;
        let result = send(payload, std::ptr::null_mut());
        assert!(take_error(result).contains("result_json pointer is null"));
    }

    #[test]
    fn send_with_invalid_payload_leaves_no_result() {
        let mut out: *mut c_char = std::ptr::dangling_mut();
        let result = send(b"{not json", &mut out);
        assert!(take_error(result).contains("invalid signal JSON"));
        assert!(out.is_null());
    }

    #[test]
    fn unknown_action_in_json_is_an_error() {
        let payload = br#"{"dedup_key":"k","action":"close"}"#;
        assert!(take_error(notify(payload)).contains("invalid signal JSON"));
    }

    #[test]
    fn resolve_without_a_key_needs_the_alert_fields() {
        let payload = br#"{"action":"resolve"}"#;
        assert!(take_error(notify(payload)).contains("must not be empty"));
    }

    #[test]
    fn freeing_a_null_string_is_a_no_op() {
        unsafe { signal_free_string(std::ptr::null_mut()) };
    }

    #[test]
    fn valid_payload_is_accepted_by_the_configured_client() {
        // The only test that reaches client(), so it alone decides which
        // config the process-global client is built from.
        let dir = std::env::temp_dir().join(format!("opssignal-ffi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("signal.yaml");
        std::fs::write(
            &config,
            format!(
                "storage:\n  path: \"{}\"\n",
                dir.join("signals.db").display()
            ),
        )
        .expect("write config");
        std::env::set_var("SIGNAL_CONFIG", &config);

        let payload = br#"{"source":"ci","event_type":"build","title":"ok","severity":"info"}"#;
        let result = notify(payload);

        assert!(result.success);
        assert!(result.error.is_null());
        // An acknowledge or resolve may name its alert by key alone.
        let resolve = br#"{"dedup_key":"k","action":"resolve"}"#;
        assert!(notify(resolve).success);
        assert!(signal_flush(5_000).success);

        let mut out: *mut c_char = std::ptr::null_mut();
        let result = send(payload, &mut out);
        assert!(result.success);
        assert!(!out.is_null());
        let json = unsafe { CStr::from_ptr(out) }
            .to_str()
            .expect("UTF-8")
            .to_owned();
        unsafe { signal_free_string(out) };
        let report: serde_json::Value = serde_json::from_str(&json).expect("result is JSON");
        assert_eq!(report["status"], "delivered");
        assert_eq!(report["provider"], "console");
        assert_eq!(report["deliveries"][0]["status"], "delivered");
        assert!(report["signal_id"]
            .as_str()
            .is_some_and(|id| id.len() == 36));
        assert!(report["dedup_key"].is_string());
    }
}
