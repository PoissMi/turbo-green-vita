//! Throttled logging for the paths that run every frame.
//!
//! A log line in the middle of a pump cycle is not free. On the Vita every
//! line is a write syscall on a runtime that has no fast console behind
//! it, and the worst offender was the "decoder queue is full" message in
//! the RTP path: it fires once per dropped frame - which is exactly the
//! moment the pipeline is already struggling. The log then made the stall
//! worse, and the worse stall produced more drops, and so more log lines.
//! That feedback loop is the most plausible reading of "the lag came much
//! faster with the logger on".
//!
//! `Throttle` keeps the first occurrence of a message and then lets at most
//! one through per interval. The numbers that matter are already counted
//! in the metrics (`q:` for the decoder queue, `drop:` for dropped frames);
//! the log only says *what* happened, so once every few seconds is enough.

use std::fmt::Arguments;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long a throttled message stays quiet after it was printed.
pub(crate) const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// Lets at most one message through per interval.
pub(crate) struct Throttle {
    /// Microseconds (since process start) before which nothing is printed.
    next_allowed_us: AtomicU64,
    interval_us: u64,
}

impl Throttle {
    pub(crate) const fn new(interval: Duration) -> Self {
        Self {
            next_allowed_us: AtomicU64::new(0),
            interval_us: interval.as_micros() as u64,
        }
    }

    /// True when the caller may print now.
    pub(crate) fn should_log(&self) -> bool {
        let now = micros_since_start();
        if now < self.next_allowed_us.load(Ordering::Relaxed) {
            return false;
        }
        self.next_allowed_us
            .store(now + self.interval_us, Ordering::Relaxed);
        true
    }
}

/// Microseconds since the first call, so a throttle can be compared
/// without holding an `Instant` behind a lock.
fn micros_since_start() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_micros() as u64
}

/// Prints `args` unless the throttle is still cooling down.
pub(crate) fn throttled(throttle: &Throttle, args: Arguments<'_>) {
    if throttle.should_log() {
        eprintln!("{args}");
    }
}
