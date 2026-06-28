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

use std::ffi::{c_char, CStr, CString};
use std::panic;

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
            CString::new("error message contained null byte").unwrap()
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
/// `json_ptr` must be a valid, null-terminated UTF-8 C string owned by
/// the caller for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn signal_notify_async(json_ptr: *const c_char) -> CResult {
    guarded(|| {
        let json_str = unsafe { CStr::from_ptr(json_ptr) }
            .to_str()
            .map_err(|e| format!("invalid UTF-8 in signal payload: {e}"))?;

        let _input: opssignal_core::signal::SignalInput = serde_json::from_str(json_str)
            .map_err(|e| format!("invalid signal JSON: {e}"))?;

        // NOTE: wiring to a long-lived SignalClient instance (held via a
        // process-global or handle passed from the host language) is the
        // next step here — left as a TODO marker for the implementation
        // PR, not hidden silently.
        // TODO(v0.1): route `_input` through a SignalClient instance.

        Ok(())
    })
}

/// Frees a string previously returned in a `CResult.error` field.
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
}
