//! Monotonic time.

/// Monotonic milliseconds. Core code never reads a clock; callers inject `now`.
pub type Ms = u64;
