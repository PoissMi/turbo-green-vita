//! Master switch for all diagnostic logging (`eprintln!` output to stderr).
//!
//! Every diagnostic goes through the [`elog!`] macro, which checks this gate
//! before touching stderr. The gate starts **off** and is turned on by
//! `Settings::load` applying the persisted `enable_logging` value in
//! `App::new` (on by default for fresh installs). Toggling it in the
//! Settings screen applies immediately without a restart.
//!
//! The streaming HUD (the green overlay on the `Streaming` screen) is a
//! separate, in-memory metric feed; it never routes through this gate, so it
//! keeps working with logging off, and in fact keeps working whenever
//! `show_stream_debug_info` is on regardless of this setting.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether diagnostic logging is enabled. Starts **off**: the crate's first
/// diagnostics happen during startup, before settings are loaded, and the
/// toggle's whole point is to silence logging completely. `App::new`
/// applies the persisted setting as soon as it is available.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Applies the persisted setting. Called once at startup (after settings
/// load) and on every toggle in the Settings screen.
pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether diagnostic logging is currently enabled.
#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Emits `args` to stderr, but only when logging is enabled.
///
/// This is the single choke point for all diagnostic output in the crate —
/// every `eprintln!` goes through [`elog!`] so the Settings toggle can silence
/// all of it at once.
pub(crate) fn log(args: std::fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    let mut line = String::new();
    let _ = line.write_fmt(args);
    eprintln!("{line}");
}

/// Formats and prints a diagnostic line — the crate-wide replacement for
/// `eprintln!`, gated by the logging setting in `Settings`.
///
/// Usage is identical to `eprintln!`:
/// `elog!("failed to start: {error:#}")`.
#[macro_export]
macro_rules! elog {
    ($($arg:tt)*) => {
        $crate::logging::log(format_args!($($arg)*))
    };
}
