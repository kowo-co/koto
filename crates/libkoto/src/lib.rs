//! Minimal, stable C ABI entry point. Execution is intentionally not exposed
//! until the backend ownership and session lifetime contracts are finalized.

use std::ffi::{CStr, c_char, c_int};

/// ABI version, incremented only for incompatible C header changes.
#[unsafe(no_mangle)]
pub extern "C" fn koto_abi_version() -> c_int {
    1
}

/// Validate a basm source buffer without accessing a compositor.
/// Returns 0 on success and 8 for a basm parse error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn koto_parse_basm(source: *const c_char) -> c_int {
    if source.is_null() {
        return 8;
    }
    let source = unsafe { CStr::from_ptr(source) };
    let Ok(source) = source.to_str() else {
        return 8;
    };
    match koto_core::parse_script(source) {
        Ok(_) => 0,
        Err(_) => 8,
    }
}
