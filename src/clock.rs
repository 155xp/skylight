//! One server clock for every timestamp that crosses the wire: microseconds since start.

use std::sync::OnceLock;
use std::time::Instant;

fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

pub fn init() {
    start();
}

pub fn now_us() -> u64 {
    start().elapsed().as_micros() as u64
}

/// CLOCK_MONOTONIC in nanoseconds, the clock PipeWire stamps buffers with.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
