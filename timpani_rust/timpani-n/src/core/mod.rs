/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

//! Core BPF management and utilities
//!
//! This module handles BPF program management, event processing, and time
//! calibration. It is the Rust port of the C `trace_bpf.c` implementation.
//!
//! BPF uses CLOCK_MONOTONIC for timestamps, but we need CLOCK_REALTIME for
//! absolute time references. This module calibrates the offset between these
//! two clocks to enable accurate timestamp conversion.
//!
//! Note: The actual BPF C programs (`*.bpf.c`) are kept in a separate `bpf/`
//! folder and compiled as-is (not ported to Rust).
//!
//!

use std::sync::atomic::{AtomicI64, Ordering};
use tracing::{debug, info};

use crate::error::{TimpaniError, TimpaniResult};

/// BPF time offset (CLOCK_MONOTONIC → CLOCK_REALTIME conversion)
/// Stored as nanoseconds to add to BPF monotonic timestamps
static BPF_KTIME_OFFSET: AtomicI64 = AtomicI64::new(0);

/// Number of calibration samples to take
const CALIBRATION_SAMPLES: usize = 20;

/// Calibrate BPF time offset by finding the offset between
/// CLOCK_MONOTONIC (used by BPF) and CLOCK_REALTIME.
///
/// This function takes multiple samples and uses the one with the smallest
/// delta (fastest measurement) to minimize timing jitter and context switch impact..
///
/// # Returns
/// - `Ok(())` on successful calibration
/// - `Err(TimpaniError::Io)` if clock_gettime fails
pub fn calibrate_time_offset() -> TimpaniResult<()> {
    let mut best_delta = u64::MAX;
    let mut best_offset: i64 = 0;

    for i in 1..=CALIBRATION_SAMPLES {
        // Get timestamps
        // print!("Calibration attempt {}: \n", i);
        let t1 = get_realtime_ns()?;
        let t2 = get_monotonic_ns()?;
        let t3 = get_realtime_ns()?;

        let delta = t3 - t1;
        let ts = (t3 + t1) / 2; // Midpoint = best estimate of "now"

        if delta < best_delta {
            best_delta = delta;
            best_offset = ts as i64 - t2 as i64;

            debug!(
                "BPF ktime calibration attempt {}: t1={}.{:09}, t2={}.{:09}, t3={}.{:09}",
                i,
                t1 / 1_000_000_000,
                t1 % 1_000_000_000,
                t2 / 1_000_000_000,
                t2 % 1_000_000_000,
                t3 / 1_000_000_000,
                t3 % 1_000_000_000
            );
            debug!(
                "Attempt {}: delta={} ns, bpf_ktime_off={} ns",
                i, delta, best_offset
            );
        }
    }

    BPF_KTIME_OFFSET.store(best_offset, Ordering::Relaxed);

    info!(
        "BPF time offset calibrated: {} ns (best delta: {} ns)",
        best_offset, best_delta
    );

    Ok(())
}

/// Convert BPF monotonic timestamp to realtime timestamp
///
/// # Arguments
/// * `bpf_ts` - BPF timestamp in nanoseconds (CLOCK_MONOTONIC)
///
/// # Returns
/// Realtime timestamp in nanoseconds (CLOCK_REALTIME)
#[inline]
pub fn bpf_ktime_to_realtime(bpf_ts: u64) -> u64 {
    let offset = BPF_KTIME_OFFSET.load(Ordering::Relaxed);
    (bpf_ts as i64 + offset) as u64
}

/// Get current CLOCK_REALTIME in nanoseconds
fn get_realtime_ns() -> TimpaniResult<u64> {
    let ts = unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) != 0 {
            return Err(TimpaniError::Io);
        }
        ts
    };
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

/// Get current CLOCK_MONOTONIC in nanoseconds
fn get_monotonic_ns() -> TimpaniResult<u64> {
    let ts = unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) != 0 {
            return Err(TimpaniError::Io);
        }
        ts
    };
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calibration_completes() {
        // Should complete without error
        assert!(calibrate_time_offset().is_ok());
    }

    #[test]
    fn test_offset_is_set() {
        calibrate_time_offset().unwrap();
        let offset = BPF_KTIME_OFFSET.load(Ordering::Relaxed);
        // Offset should be non-zero after calibration
        assert_ne!(offset, 0);
    }

    #[test]
    fn test_conversion_produces_valid_timestamp() {
        calibrate_time_offset().unwrap();

        let rt = get_realtime_ns().unwrap();
        let mono = get_monotonic_ns().unwrap();
        let bpf_rt = bpf_ktime_to_realtime(mono);

        // Converted time should be close to actual realtime (within 1ms)
        let diff = (rt as i64 - bpf_rt as i64).abs();
        assert!(
            diff < 1_000_000,
            "Converted time differs from realtime by {} ns (> 1ms)",
            diff
        );
    }

    #[test]
    fn test_get_realtime_ns() {
        let rt = get_realtime_ns().unwrap();
        // Should be a reasonable timestamp (after year 2000)
        assert!(rt > 946_684_800_000_000_000); // Jan 1, 2000 in ns
    }

    #[test]
    fn test_get_monotonic_ns() {
        let mono1 = get_monotonic_ns().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mono2 = get_monotonic_ns().unwrap();
        // Monotonic should increase
        assert!(mono2 > mono1);
    }

    #[test]
    fn test_multiple_calibrations() {
        // Should be safe to calibrate multiple times
        assert!(calibrate_time_offset().is_ok());
        let offset1 = BPF_KTIME_OFFSET.load(Ordering::Relaxed);

        assert!(calibrate_time_offset().is_ok());
        let offset2 = BPF_KTIME_OFFSET.load(Ordering::Relaxed);

        // Offsets should be very close (within 1ms)
        let diff = (offset1 - offset2).abs();
        assert!(
            diff < 1_000_000,
            "Calibration offsets differ by {} ns",
            diff
        );
    }
}
