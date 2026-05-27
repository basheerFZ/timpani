/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

//! Per-task runtime state and task list management.
//!
//! This module mirrors `task.c` from the C implementation.
//!
//! Each scheduled task is represented by a [`TimeTrigger`], which bundles the
//! static scheduling parameters ([`crate::context::TaskInfo`]) with the
//! runtime handles needed to signal it (PID, pidfd) and the atomic state
//! shared with the BPF ring-buffer callback ([`BpfState`]).
//!
//! [`init_task_list`] walks `sched_info.tasks`, finds each process by name,
//! applies affinity and scheduling attributes, and opens a pidfd.  The
//! resulting `Vec<TimeTrigger>` is stored in
//! [`crate::context::RuntimeState::tt_list`].
//!
//! Teardown is implicit: dropping the `Vec<TimeTrigger>` closes every
//! `OwnedFd` (pidfd) automatically.  [`teardown_task_list`] exists only to
//! make the intent explicit at call sites.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use nix::unistd::Pid;
use tracing::{info, warn};

use crate::context::{SchedInfo, TaskInfo};
use crate::error::TimpaniResult;
use crate::sched::{
    create_pidfd, get_pid_by_name, set_affinity_cpumask, set_schedattr, SchedPolicy,
};

// ── BpfState ──────────────────────────────────────────────────────────────────

/// Per-task atomic state shared between the timer callback and the BPF
/// ring-buffer callback.
///
/// Mirrors `sigwait_ts` and `sigwait_enter` in `struct time_trigger` (C).
/// Wrapped in [`Arc`] so the BPF ring-buffer consumer and the timer loop can
/// each hold a reference without copying the atomics.
#[derive(Debug)]
pub struct BpfState {
    /// Monotonic timestamp (nanoseconds) recorded when the task most recently
    /// entered `sigwaitinfo`.  Written by the BPF ring-buffer callback,
    /// read by the timer callback to detect stuck-in-kernel tasks.
    pub sigwait_ts: AtomicU64,

    /// `true` while the task is blocked inside `sigwaitinfo`, `false` once it
    /// returns.  Written by the BPF ring-buffer callback, read by the timer
    /// callback to skip the signal if the task is not yet waiting.
    pub sigwait_enter: AtomicBool,
}

impl BpfState {
    pub fn new() -> Self {
        Self {
            sigwait_ts: AtomicU64::new(0),
            sigwait_enter: AtomicBool::new(false),
        }
    }
}

impl Default for BpfState {
    fn default() -> Self {
        Self::new()
    }
}

// ── TimeTrigger ───────────────────────────────────────────────────────────────

/// Runtime entry for one scheduled task.
///
/// Mirrors `struct time_trigger` from `timetrigger.h` / `task.c` in the C
/// implementation.  Created by [`init_task_list`] at startup and on every
/// workload change.
#[derive(Debug)]
pub struct TimeTrigger {
    /// Static scheduling parameters received from Timpani-O.
    pub info: TaskInfo,

    /// Host PID of the task process, located by [`get_pid_by_name`].
    pub pid: Pid,

    /// A pidfd for `pid`.  Used for race-free signal delivery via
    /// `pidfd_send_signal(2)`.  Closed automatically on drop.
    pub pidfd: OwnedFd,

    /// Atomic state shared with the BPF ring-buffer callback.
    pub bpf_state: Arc<BpfState>,

    /// Timestamp of the most recent timer fire for this task.
    /// `None` until the first activation.  Used to log jitter.
    pub prev_timer: Option<Instant>,

    /// `sigwait_ts` value observed on the previous timer tick.
    /// Compared to the current value to detect tasks stuck in the kernel
    /// (the timestamp does not advance if the BPF probe never fires).
    pub sigwait_ts_prev: u64,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build the runtime task list from a schedule received from Timpani-O.
///
/// For each task in `sched_info.tasks`:
/// 1. Locate the process by name via `/proc` scan ([`get_pid_by_name`]).
/// 2. Apply CPU affinity ([`set_affinity_cpumask`]) unless `cpu_affinity == 0`
///    ("any CPU").
/// 3. Apply scheduling policy and priority ([`set_schedattr`]).
/// 4. Open a pidfd ([`create_pidfd`]) for race-free signal delivery.
///
/// Returns an error if any step fails for any task.  The caller should treat
/// this as a fatal startup error and abort.
///
/// Mirrors `init_task_list()` in `task.c`, minus the node_id filter — the
/// gRPC server already returns only this node's tasks.
pub fn init_task_list(sched_info: &SchedInfo) -> TimpaniResult<Vec<TimeTrigger>> {
    let mut tt_list = Vec::with_capacity(sched_info.tasks.len());

    for task_info in &sched_info.tasks {
        // Step 1: find the process in /proc by comm name.
        let pid = get_pid_by_name(&task_info.name).inspect_err(|_| {
            warn!(
                name = %task_info.name,
                "init_task_list: process not found"
            );
        })?;

        // Step 2: apply CPU affinity (0 means "any CPU" — leave as-is).
        if task_info.cpu_affinity != 0 {
            set_affinity_cpumask(pid, task_info.cpu_affinity).inspect_err(|_| {
                warn!(
                    name    = %task_info.name,
                    pid     = %pid,
                    cpumask = task_info.cpu_affinity,
                    "init_task_list: set_affinity_cpumask failed"
                );
            })?;
        }

        // Step 3: apply scheduling policy and priority.
        let policy = SchedPolicy::try_from(task_info.sched_policy as i32).inspect_err(|_| {
            warn!(
                name   = %task_info.name,
                policy = task_info.sched_policy,
                "init_task_list: unknown sched_policy"
            );
        })?;
        set_schedattr(pid, task_info.sched_priority, policy).inspect_err(|_| {
            warn!(
                name     = %task_info.name,
                pid      = %pid,
                priority = task_info.sched_priority,
                ?policy,
                "init_task_list: set_schedattr failed"
            );
        })?;

        // Step 4: open a pidfd for race-free signal delivery.
        let pidfd = create_pidfd(pid).inspect_err(|_| {
            warn!(
                name = %task_info.name,
                pid  = %pid,
                "init_task_list: create_pidfd failed"
            );
        })?;

        info!(
            name      = %task_info.name,
            pid       = %pid,
            ?policy,
            priority  = task_info.sched_priority,
            period_us = task_info.period_us,
            "Task initialized"
        );

        tt_list.push(TimeTrigger {
            info: task_info.clone(),
            pid,
            pidfd,
            bpf_state: Arc::new(BpfState::default()),
            prev_timer: None,
            sigwait_ts_prev: 0,
        });
    }

    info!(
        task_count   = tt_list.len(),
        workload_id  = %sched_info.workload_id,
        "Task list initialized"
    );
    Ok(tt_list)
}

/// Tear down the task list, releasing all pidfds.
///
/// This is a no-op beyond consuming the `Vec`: every [`TimeTrigger`] holds an
/// [`OwnedFd`] whose `Drop` impl closes the underlying pidfd automatically.
/// The function exists to make teardown intent explicit at call sites and to
/// provide a hook for future cleanup logic.
pub fn teardown_task_list(_tt_list: Vec<TimeTrigger>) {
    // OwnedFd pidfds are closed on drop.
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn bpf_state_initializes_to_zero() {
        let s = BpfState::new();
        assert_eq!(s.sigwait_ts.load(Ordering::Relaxed), 0);
        assert!(!s.sigwait_enter.load(Ordering::Relaxed));
    }

    #[test]
    fn bpf_state_default_equals_new() {
        let a = BpfState::default();
        let b = BpfState::new();
        assert_eq!(
            a.sigwait_ts.load(Ordering::Relaxed),
            b.sigwait_ts.load(Ordering::Relaxed)
        );
        assert_eq!(
            a.sigwait_enter.load(Ordering::Relaxed),
            b.sigwait_enter.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn teardown_empty_list_does_not_panic() {
        teardown_task_list(vec![]);
    }

    #[test]
    fn init_task_list_empty_sched_info_returns_empty_vec() {
        use crate::context::SchedInfo;
        use std::time::Instant;

        let sched_info = SchedInfo {
            workload_id: "test-wl".to_string(),
            hyperperiod_us: 100_000,
            tasks: vec![],
            received_at: Instant::now(),
        };
        let result = init_task_list(&sched_info).expect("empty task list must succeed");
        assert!(result.is_empty());
    }

    #[test]
    fn init_task_list_unknown_process_returns_error() {
        use crate::context::{SchedInfo, TaskInfo};
        use std::time::Instant;

        let sched_info = SchedInfo {
            workload_id: "test-wl".to_string(),
            hyperperiod_us: 100_000,
            tasks: vec![TaskInfo {
                name: "__no_such_process__".to_string(),
                sched_policy: 0,
                sched_priority: 0,
                period_us: 10_000,
                release_time_us: 0,
                runtime_us: 1_000,
                deadline_us: 10_000,
                cpu_affinity: 0,
                max_dmiss: 0,
            }],
            received_at: Instant::now(),
        };
        let result = init_task_list(&sched_info);
        assert!(result.is_err(), "unknown process must return an error");
    }

    // ── Helpers for tests that need the current process ───────────────────────

    fn self_task_info(
        cpu_affinity: u64,
        sched_policy: u32,
        sched_priority: u32,
    ) -> crate::context::TaskInfo {
        let pid = nix::unistd::Pid::from_raw(std::process::id() as i32);
        let name =
            crate::sched::get_process_name_by_pid(pid).expect("must be able to read own comm name");
        crate::context::TaskInfo {
            name,
            sched_policy,
            sched_priority,
            period_us: 10_000,
            release_time_us: 0,
            runtime_us: 1_000,
            deadline_us: 10_000,
            cpu_affinity,
            max_dmiss: 0,
        }
    }

    fn make_sched(tasks: Vec<crate::context::TaskInfo>) -> crate::context::SchedInfo {
        crate::context::SchedInfo {
            workload_id: "test-wl".to_string(),
            hyperperiod_us: 100_000,
            tasks,
            received_at: std::time::Instant::now(),
        }
    }

    // ── C: init_task_list with real (current) process ─────────────────────────

    #[test]
    fn init_task_list_with_self_zero_affinity_succeeds() {
        let sched_info = make_sched(vec![self_task_info(0, 0, 0)]);
        let tt = init_task_list(&sched_info).expect("must succeed for the current process");
        assert!(!tt.is_empty(), "TimeTrigger list must be non-empty");
        assert!(tt[0].pid.as_raw() > 0, "returned PID must be positive");
    }

    #[test]
    fn init_task_list_with_self_pidfd_is_alive() {
        use std::os::fd::AsFd;
        let sched_info = make_sched(vec![self_task_info(0, 0, 0)]);
        let tt = init_task_list(&sched_info).expect("must succeed");
        assert!(
            crate::sched::is_process_alive(tt[0].pidfd.as_fd()),
            "pidfd must refer to a live process"
        );
    }

    #[test]
    fn init_task_list_zero_affinity_does_not_change_affinity() {
        // cpu_affinity=0 means "no pinning" — set_affinity_cpumask must NOT be called.
        // If it were called with mask=0 it would return InvalidArgs, causing an Err here.
        let sched_info = make_sched(vec![self_task_info(0, 0, 0)]);
        assert!(
            init_task_list(&sched_info).is_ok(),
            "cpu_affinity=0 must not invoke set_affinity_cpumask (which rejects mask=0)"
        );
    }

    #[test]
    fn init_task_list_with_self_applies_affinity() {
        let self_pid = nix::unistd::Pid::from_raw(std::process::id() as i32);
        let original = nix::sched::sched_getaffinity(self_pid).ok();

        let sched_info = make_sched(vec![self_task_info(0x1, 0, 0)]); // pin to CPU 0
        if let Ok(tt) = init_task_list(&sched_info) {
            // Read affinity on the exact TID that was found and configured.
            let tid = tt[0].pid;
            if let Ok(mask) = nix::sched::sched_getaffinity(tid) {
                assert!(
                    matches!(mask.is_set(0), Ok(true)),
                    "CPU 0 must be set after init with cpu_affinity=0x1"
                );
                let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as usize;
                for i in 1..num_cpus.min(64) {
                    assert!(
                        matches!(mask.is_set(i), Ok(false)),
                        "CPU {i} must not be set when cpu_affinity=0x1"
                    );
                }
            }
        }

        // Restore the main thread's original affinity.
        if let Some(ref orig) = original {
            let _ = nix::sched::sched_setaffinity(self_pid, orig);
        }
    }

    #[test]
    #[ignore = "requires CAP_SYS_NICE; run with: cargo test -- --ignored"]
    fn init_task_list_with_self_applies_schedattr() {
        let sched_info = make_sched(vec![self_task_info(0, 1, 50)]); // SCHED_FIFO prio 50
        let tt = init_task_list(&sched_info).expect("CAP_SYS_NICE required");
        let tid = tt[0].pid;
        let policy = unsafe { libc::sched_getscheduler(tid.as_raw()) };
        assert_eq!(policy, libc::SCHED_FIFO, "expected SCHED_FIFO");
        let mut param = libc::sched_param { sched_priority: 0 };
        unsafe { libc::sched_getparam(tid.as_raw(), &mut param) };
        assert_eq!(param.sched_priority, 50, "expected priority 50");
        // Restore to SCHED_NORMAL
        let _ = crate::sched::set_schedattr(tid, 0, crate::sched::SchedPolicy::Normal);
    }

    #[test]
    fn init_task_list_invalid_policy_returns_error() {
        // cpu_affinity=0 skips affinity step; sched_policy=99 is invalid → step 3 fails.
        let sched_info = make_sched(vec![self_task_info(0, 99, 0)]);
        assert!(
            matches!(
                init_task_list(&sched_info),
                Err(crate::error::TimpaniError::InvalidArgs)
            ),
            "sched_policy=99 must produce TimpaniError::InvalidArgs"
        );
    }

    // ── C7: Arc<BpfState> shared-write semantics ─────────────────────────────

    #[test]
    fn bpf_state_arc_write_visible_across_clone() {
        let state = Arc::new(BpfState::new());
        let clone = Arc::clone(&state);
        state.sigwait_ts.store(99_999, Ordering::SeqCst);
        state.sigwait_enter.store(true, Ordering::SeqCst);
        assert_eq!(clone.sigwait_ts.load(Ordering::SeqCst), 99_999);
        assert!(clone.sigwait_enter.load(Ordering::SeqCst));
    }
}
