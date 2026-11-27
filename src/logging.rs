//! Console diagnostics output, controlled by the Settings toggle.
//!
//! The debug HUD is built independently from metrics and is not affected
//! by this flag: with the HUD on and logging off, the HUD still renders.

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether console output is allowed. Set from the Settings toggle (and once
/// at app startup from the persisted value).
static LOGGING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Sets the flag. A plain store on a single atomic, so it is safe from
/// any thread.
pub(crate) fn set_enabled(enabled: bool) {
    LOGGING_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether console output may happen right now.
pub(crate) fn middleware_enabled() -> bool {
    LOGGING_ENABLED.load(Ordering::Relaxed)
}

/// The formatted console line that used to be printed unconditionally.
macro_rules! log_line {
    ($($arg:tt)*) => {
        if crate::logging::middleware_enabled() {
            eprintln!($($arg)*);
        }
    };
}

pub(crate) use log_line;
