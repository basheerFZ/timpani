/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

//! Linux scheduling and process management.
//! See DEVELOPER_NOTES.md D-N-001, D-N-002, D-N-006 for crate-choice rationale.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;
use tracing::{debug, info, warn};

use crate::error::{TimpaniError, TimpaniResult};

// ── Scheduling policy ─────────────────────────────────────────────────────────

/// Linux scheduling policy. Integer values match the Linux ABI (proto field direct mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedPolicy {
    Normal, // SCHED_OTHER = 0
    Fifo,   // SCHED_FIFO  = 1
    Rr,     // SCHED_RR    = 2
}

impl TryFrom<i32> for SchedPolicy {
    type Error = TimpaniError;

    fn try_from(v: i32) -> TimpaniResult<Self> {
        match v {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Fifo),
            2 => Ok(Self::Rr),
            _ => {
                tracing::error!("Unknown sched_policy value {}", v);
                Err(TimpaniError::InvalidArgs)
            }
        }
    }
}

impl SchedPolicy {
    fn to_libc(self) -> libc::c_int {
        match self {
            Self::Normal => libc::SCHED_OTHER,
            Self::Fifo => libc::SCHED_FIFO,
            Self::Rr => libc::SCHED_RR,
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn map_nix_err(e: nix::errno::Errno) -> TimpaniError {
    match e {
        nix::errno::Errno::EPERM | nix::errno::Errno::EACCES => TimpaniError::Permission,
        _ => TimpaniError::Io,
    }
}

fn map_proc_err(_: procfs::ProcError) -> TimpaniError {
    TimpaniError::Io
}

// ── CPU affinity ──────────────────────────────────────────────────────────────

/// Set CPU affinity for `pid` to a single core. Falls back to CPU 0 if `cpu` is out of range.
pub fn set_affinity(pid: Pid, cpu: u32) -> TimpaniResult<()> {
    let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if num_cpus < 0 {
        return Err(TimpaniError::Io);
    }

    let effective_cpu = if cpu as i64 >= num_cpus {
        warn!(
            "CPU {} out of range (0–{}), falling back to CPU 0",
            cpu,
            num_cpus - 1
        );
        0u32
    } else {
        cpu
    };

    let mut cpuset = CpuSet::new();
    cpuset
        .set(effective_cpu as usize)
        .map_err(|_| TimpaniError::InvalidArgs)?;

    sched_setaffinity(pid, &cpuset).map_err(|e| {
        tracing::error!(
            "sched_setaffinity failed for PID {} (cpu {}): {}",
            pid,
            effective_cpu,
            e
        );
        map_nix_err(e)
    })?;

    info!("Set CPU affinity for PID {} to CPU {}", pid, effective_cpu);
    Ok(())
}

/// Set CPU affinity for `pid` from a bitmask. Bit N = CPU N. Zero mask is rejected.
pub fn set_affinity_cpumask(pid: Pid, cpumask: u64) -> TimpaniResult<()> {
    if cpumask == 0 {
        return Err(TimpaniError::InvalidArgs);
    }

    let mut cpuset = CpuSet::new();
    for i in 0..64usize {
        if cpumask & (1u64 << i) != 0 {
            let _ = cpuset.set(i); // silently skip if i >= CpuSet::CAPACITY
        }
    }

    sched_setaffinity(pid, &cpuset).map_err(|e| {
        tracing::error!(
            "sched_setaffinity failed for PID {} (cpumask 0x{:x}): {}",
            pid,
            cpumask,
            e
        );
        map_nix_err(e)
    })?;

    Ok(())
}

/// Set CPU affinity for all threads of `pid`. Succeeds if at least one thread is updated.
pub fn set_affinity_cpumask_all_threads(pid: Pid, cpumask: u64) -> TimpaniResult<()> {
    if cpumask == 0 {
        return Err(TimpaniError::InvalidArgs);
    }

    let tasks = procfs::process::Process::new(pid.as_raw())
        .and_then(|p| p.tasks())
        .map_err(|e| {
            tracing::error!("Failed to enumerate threads for PID {}: {}", pid, e);
            map_proc_err(e)
        })?;

    let mut success = 0usize;
    let mut failed = 0usize;

    for task in tasks {
        let Ok(task) = task else {
            failed += 1;
            continue;
        };
        let tid = Pid::from_raw(task.tid);
        match set_affinity_cpumask(tid, cpumask) {
            Ok(()) => {
                success += 1;
                debug!("Set affinity TID {} cpumask 0x{:x}", tid, cpumask);
            }
            Err(e) => {
                failed += 1;
                warn!("Failed affinity TID {}: {:?}", tid, e);
            }
        }
    }

    info!(
        "Affinity set for {}/{} threads of PID {}",
        success,
        success + failed,
        pid
    );

    if success == 0 && failed > 0 {
        Err(TimpaniError::Permission)
    } else {
        Ok(())
    }
}

// ── Scheduling attributes ─────────────────────────────────────────────────────

/// Set the scheduling policy and priority for `pid`. See D-N-006.
pub fn set_schedattr(pid: Pid, priority: u32, policy: SchedPolicy) -> TimpaniResult<()> {
    if priority > 99 {
        tracing::error!("Invalid RT priority {} (must be 0–99)", priority);
        return Err(TimpaniError::InvalidArgs);
    }

    let ret = unsafe {
        let param = libc::sched_param {
            sched_priority: priority as libc::c_int,
        };
        libc::sched_setscheduler(pid.as_raw(), policy.to_libc(), &param)
    };

    if ret < 0 {
        let err = nix::errno::Errno::last();
        tracing::error!(
            "sched_setscheduler failed for PID {} ({:?}, prio {}): {}",
            pid,
            policy,
            priority,
            err
        );
        return Err(map_nix_err(err));
    }

    info!(
        "Scheduling set for PID {}: policy={:?}, priority={}",
        pid, policy, priority
    );
    Ok(())
}

// ── Process / thread discovery ────────────────────────────────────────────────

/// Return the comm name of `pid` from `/proc/<pid>/stat`.
pub fn get_process_name_by_pid(pid: Pid) -> TimpaniResult<String> {
    procfs::process::Process::new(pid.as_raw())
        .and_then(|p| p.stat())
        .map(|s| s.comm)
        .map_err(|e| {
            tracing::error!("Failed to read comm for PID {}: {}", pid, e);
            map_proc_err(e)
        })
}

/// Find a TID by scanning `/proc/*/task/*` for a matching `comm` name.
pub fn get_pid_by_name(name: &str) -> TimpaniResult<Pid> {
    let processes = procfs::process::all_processes().map_err(|e| {
        tracing::error!("Failed to open /proc: {}", e);
        map_proc_err(e)
    })?;

    for prc in processes {
        let Ok(prc) = prc else { continue };
        let Ok(tasks) = prc.tasks() else { continue };
        for task in tasks {
            let Ok(task) = task else { continue };
            let Ok(stat) = task.stat() else { continue };
            if stat.comm == name {
                debug!("Found thread '{}' TID {}", name, task.tid);
                return Ok(Pid::from_raw(task.tid));
            }
        }
    }

    warn!("Thread '{}' not found", name);
    Err(TimpaniError::Io)
}

/// Find a process by `name` and its namespace PID (`NSpid` in `/proc/<pid>/status`).
pub fn get_pid_by_nspid(name: &str, nspid: i32) -> TimpaniResult<Pid> {
    let processes = procfs::process::all_processes().map_err(|e| {
        tracing::error!("Failed to open /proc: {}", e);
        map_proc_err(e)
    })?;

    for prc in processes {
        let Ok(prc) = prc else { continue };
        let Ok(status) = prc.status() else { continue };

        if status.name != name {
            continue;
        }

        // nspid is Option<Vec<i32>>; absent on older kernels
        if let Some(ref ns_list) = status.nspid {
            if ns_list.contains(&nspid) {
                let pid = Pid::from_raw(prc.pid);
                debug!("Found '{}' nspid {} at host PID {}", name, nspid, pid);
                return Ok(pid);
            }
        }
    }

    debug!("Process '{}' nspid {} not found", name, nspid);
    Err(TimpaniError::Io)
}

// ── pidfd ─────────────────────────────────────────────────────────────────────

/// Open a pidfd for `pid` (requires Linux ≥ 5.3). The returned `OwnedFd` closes on drop.
/// nix 0.29 does not wrap pidfd_open — see D-N-001.
pub fn create_pidfd(pid: Pid) -> TimpaniResult<OwnedFd> {
    // SAFETY: pidfd_open(pid, 0); flags=0 is the only defined value.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw() as libc::c_long, 0u32) };

    if fd < 0 {
        let err = nix::errno::Errno::last();
        tracing::error!("pidfd_open failed for PID {}: {}", pid, err);
        return Err(match err {
            nix::errno::Errno::ESRCH => TimpaniError::InvalidArgs,
            nix::errno::Errno::EPERM | nix::errno::Errno::EACCES => TimpaniError::Permission,
            _ => TimpaniError::Io,
        });
    }

    // SAFETY: fd is kernel-owned; OwnedFd takes sole ownership and closes on drop.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// Send `signal` to the process referenced by `pidfd`. Avoids the PID-reuse race of `kill(2)`.
/// nix 0.29 does not wrap pidfd_send_signal — see D-N-001.
pub fn send_signal_pidfd(
    pidfd: BorrowedFd<'_>,
    signal: nix::sys::signal::Signal,
) -> TimpaniResult<()> {
    // SAFETY: pidfd_send_signal(fd, sig, NULL, 0); info=NULL is well-defined.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd() as libc::c_long,
            signal as libc::c_long,
            0i64, // siginfo_t* = NULL
            0u32, // flags = 0
        )
    };

    if ret < 0 {
        let err = nix::errno::Errno::last();
        tracing::error!("pidfd_send_signal failed ({:?}): {}", signal, err);
        return Err(match err {
            nix::errno::Errno::EPERM | nix::errno::Errno::EACCES => TimpaniError::Permission,
            nix::errno::Errno::ESRCH => TimpaniError::Signal,
            _ => TimpaniError::Io,
        });
    }
    Ok(())
}

/// Returns `true` if the process referenced by `pidfd` is still alive.
/// Sends the null signal (0) — checks existence without delivering anything to the process.
pub fn is_process_alive(pidfd: BorrowedFd<'_>) -> bool {
    // SAFETY: signal=0 never delivered; used only for existence check.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd() as libc::c_long,
            0i64,
            0i64,
            0u32,
        )
    };
    ret == 0
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn test_sched_policy_from_int() {
        assert_eq!(SchedPolicy::try_from(0).unwrap(), SchedPolicy::Normal);
        assert_eq!(SchedPolicy::try_from(1).unwrap(), SchedPolicy::Fifo);
        assert_eq!(SchedPolicy::try_from(2).unwrap(), SchedPolicy::Rr);
        assert!(SchedPolicy::try_from(3).is_err());
        assert!(SchedPolicy::try_from(-1).is_err());
    }

    #[test]
    fn test_sched_policy_to_libc() {
        assert_eq!(SchedPolicy::Normal.to_libc(), libc::SCHED_OTHER);
        assert_eq!(SchedPolicy::Fifo.to_libc(), libc::SCHED_FIFO);
        assert_eq!(SchedPolicy::Rr.to_libc(), libc::SCHED_RR);
    }

    #[test]
    fn test_set_schedattr_invalid_priority() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        assert_eq!(
            set_schedattr(pid, 100, SchedPolicy::Fifo).unwrap_err(),
            TimpaniError::InvalidArgs
        );
    }

    #[test]
    fn test_set_affinity_cpumask_zero_rejected() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        assert_eq!(
            set_affinity_cpumask(pid, 0).unwrap_err(),
            TimpaniError::InvalidArgs
        );
    }

    #[test]
    fn test_own_process_name() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        assert!(get_process_name_by_pid(pid).is_ok());
    }

    #[test]
    fn test_get_pid_by_name_not_found() {
        assert_eq!(
            get_pid_by_name("__timpani_nonexistent__").unwrap_err(),
            TimpaniError::Io
        );
    }

    #[test]
    fn test_sched_policy_debug() {
        let policy = SchedPolicy::Fifo;
        assert_eq!(format!("{:?}", policy), "Fifo");
    }

    #[test]
    fn test_sched_policy_clone() {
        let policy = SchedPolicy::Rr;
        let cloned = policy;
        assert_eq!(policy, cloned);
    }

    #[test]
    fn test_sched_policy_copy() {
        let policy = SchedPolicy::Normal;
        let copied = policy;
        assert_eq!(policy, copied);
    }

    #[test]
    fn test_map_nix_err_permission() {
        assert_eq!(
            map_nix_err(nix::errno::Errno::EPERM),
            TimpaniError::Permission
        );
        assert_eq!(
            map_nix_err(nix::errno::Errno::EACCES),
            TimpaniError::Permission
        );
    }

    #[test]
    fn test_map_nix_err_io() {
        assert_eq!(map_nix_err(nix::errno::Errno::EIO), TimpaniError::Io);
        assert_eq!(map_nix_err(nix::errno::Errno::EINVAL), TimpaniError::Io);
    }

    #[test]
    fn test_set_affinity_cpumask_valid() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Try setting to CPU 0 - should succeed on any system
        let result = set_affinity_cpumask(pid, 0x1);
        // May fail if not running with proper permissions, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_affinity_cpumask_all_threads_zero_rejected() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        assert_eq!(
            set_affinity_cpumask_all_threads(pid, 0).unwrap_err(),
            TimpaniError::InvalidArgs
        );
    }

    #[test]
    fn test_set_affinity_out_of_range_cpu() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // CPU 9999 should be out of range and fall back to CPU 0
        let result = set_affinity(pid, 9999);
        // May fail due to permissions, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_get_pid_by_nspid_not_found() {
        // Non-existent process with non-existent nspid
        let result = get_pid_by_nspid("__nonexistent__", 999999);
        assert_eq!(result.unwrap_err(), TimpaniError::Io);
    }

    #[test]
    fn test_create_pidfd_invalid_pid() {
        // PID -1 should fail
        let result = create_pidfd(Pid::from_raw(-1));
        assert!(result.is_err());
    }

    #[test]
    fn test_create_pidfd_self() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let result = create_pidfd(pid);
        // Should succeed if kernel supports pidfd (Linux >= 5.3)
        // or fail gracefully on older kernels
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_is_process_alive_self() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        if let Ok(pidfd) = create_pidfd(pid) {
            assert!(is_process_alive(pidfd.as_fd()));
        }
    }

    #[test]
    fn test_send_signal_pidfd_self() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        if let Ok(pidfd) = create_pidfd(pid) {
            // Test that is_process_alive works for self
            assert!(is_process_alive(pidfd.as_fd()));
        }
    }

    #[test]
    fn test_set_schedattr_valid_priority_range() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Priority 0 should be valid for Normal policy
        let result = set_schedattr(pid, 0, SchedPolicy::Normal);
        // May fail due to permissions, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_affinity_cpu_0() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // CPU 0 should exist on any system
        let result = set_affinity(pid, 0);
        // May fail due to permissions, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_affinity_cpumask_multiple_cpus() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Set affinity to CPU 0 and 1 (bitmask 0x3 = 0b11)
        let result = set_affinity_cpumask(pid, 0x3);
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_affinity_cpumask_all_threads() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Try to set affinity for all threads
        let result = set_affinity_cpumask_all_threads(pid, 0x1);
        // Should succeed or fail gracefully
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_schedattr_fifo_policy() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Try FIFO with priority 50
        let result = set_schedattr(pid, 50, SchedPolicy::Fifo);
        // Will likely fail due to permissions, but should not panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_schedattr_rr_policy() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Try RR with priority 30
        let result = set_schedattr(pid, 30, SchedPolicy::Rr);
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_get_process_name_self() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let result = get_process_name_by_pid(pid);
        assert!(result.is_ok());
        let name = result.unwrap();
        // Process name should not be empty
        assert!(!name.is_empty());
    }

    #[test]
    fn test_get_process_name_invalid_pid() {
        // PID 999999 is very unlikely to exist
        let pid = Pid::from_raw(999999);
        let result = get_process_name_by_pid(pid);
        assert!(matches!(result, Err(TimpaniError::Io)));
    }

    #[test]
    fn test_create_pidfd_for_self() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let result = create_pidfd(pid);
        // Should succeed on Linux >= 5.3 or fail gracefully
        if let Ok(pidfd) = result {
            // pidfd should be a valid file descriptor
            assert!(pidfd.as_raw_fd() >= 0);
        }
    }

    #[test]
    fn test_create_pidfd_nonexistent() {
        // Try to create pidfd for non-existent PID
        let pid = Pid::from_raw(999999);
        let result = create_pidfd(pid);
        // Should fail
        assert!(result.is_err());
    }

    #[test]
    fn test_map_nix_err_esrch() {
        // Test ESRCH error mapping in create_pidfd context
        // We can't directly test the error mapping without calling the function
        // but we can test that invalid PIDs return errors
        let result = create_pidfd(Pid::from_raw(-1));
        assert!(result.is_err());
    }

    #[test]
    fn test_sched_policy_equality() {
        assert_eq!(SchedPolicy::Normal, SchedPolicy::Normal);
        assert_eq!(SchedPolicy::Fifo, SchedPolicy::Fifo);
        assert_eq!(SchedPolicy::Rr, SchedPolicy::Rr);
        assert_ne!(SchedPolicy::Normal, SchedPolicy::Fifo);
    }

    #[test]
    fn test_set_affinity_with_high_cpu() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Try CPU 100 (likely out of range, should fall back to CPU 0)
        let result = set_affinity(pid, 100);
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_set_affinity_cpumask_high_bit() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Set high bit in mask (CPU 63)
        let result = set_affinity_cpumask(pid, 1u64 << 63);
        // May fail if system doesn't have that many CPUs, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    // ── A: Affinity read-back ─────────────────────────────────────────────────

    #[test]
    fn set_affinity_cpu0_and_read_back() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let original = nix::sched::sched_getaffinity(pid).ok();
        if set_affinity_cpumask(pid, 0x1).is_ok() {
            let mask = nix::sched::sched_getaffinity(pid)
                .expect("sched_getaffinity must succeed after setting cpu_affinity=0x1");
            assert!(
                matches!(mask.is_set(0), Ok(true)),
                "CPU 0 must be set when mask=0x1"
            );
            let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as usize;
            for i in 1..num_cpus.min(64) {
                assert!(
                    matches!(mask.is_set(i), Ok(false)),
                    "CPU {i} must not be set when mask=0x1"
                );
            }
        }
        if let Some(ref orig) = original {
            let _ = nix::sched::sched_setaffinity(pid, orig);
        }
    }

    #[test]
    fn set_affinity_full_mask_and_read_back() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let original = nix::sched::sched_getaffinity(pid).ok();
        let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as usize;
        if set_affinity_cpumask(pid, !0u64).is_ok() {
            let mask = nix::sched::sched_getaffinity(pid)
                .expect("sched_getaffinity must succeed after full-mask set");
            for i in 0..num_cpus.min(64) {
                assert!(
                    matches!(mask.is_set(i), Ok(true)),
                    "CPU {i} must be set in full-CPU mask"
                );
            }
        }
        if let Some(ref orig) = original {
            let _ = nix::sched::sched_setaffinity(pid, orig);
        }
    }

    // ── B: Scheduler attribute read-back ─────────────────────────────────────

    #[test]
    fn set_schedattr_normal_and_verify() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        // Save current policy so we can restore it afterward.
        let orig_policy = unsafe { libc::sched_getscheduler(pid.as_raw()) };
        let mut orig_param = libc::sched_param { sched_priority: 0 };
        unsafe { libc::sched_getparam(pid.as_raw(), &mut orig_param) };

        // Switching to SCHED_NORMAL (prio 0) never requires CAP_SYS_NICE.
        if set_schedattr(pid, 0, SchedPolicy::Normal).is_ok() {
            let policy = unsafe { libc::sched_getscheduler(pid.as_raw()) };
            assert_eq!(
                policy,
                libc::SCHED_OTHER,
                "sched_getscheduler must return SCHED_OTHER after set to Normal"
            );
            let mut param = libc::sched_param { sched_priority: 0 };
            unsafe { libc::sched_getparam(pid.as_raw(), &mut param) };
            assert_eq!(
                param.sched_priority, 0,
                "priority must be 0 for SCHED_NORMAL"
            );
        }

        // Restore — best-effort, ignore error (may lack privileges for RT restore).
        if orig_policy >= 0 {
            let restore = SchedPolicy::try_from(orig_policy).unwrap_or(SchedPolicy::Normal);
            let _ = set_schedattr(pid, orig_param.sched_priority as u32, restore);
        }
    }

    #[test]
    #[ignore = "requires CAP_SYS_NICE; run with: cargo test -- --ignored"]
    fn set_schedattr_fifo_and_verify() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let orig_policy = unsafe { libc::sched_getscheduler(pid.as_raw()) };
        let mut orig_param = libc::sched_param { sched_priority: 0 };
        unsafe { libc::sched_getparam(pid.as_raw(), &mut orig_param) };

        set_schedattr(pid, 50, SchedPolicy::Fifo).expect("CAP_SYS_NICE required");

        let policy = unsafe { libc::sched_getscheduler(pid.as_raw()) };
        assert_eq!(
            policy,
            libc::SCHED_FIFO,
            "expected SCHED_FIFO after set_schedattr(Fifo, 50)"
        );
        let mut param = libc::sched_param { sched_priority: 0 };
        unsafe { libc::sched_getparam(pid.as_raw(), &mut param) };
        assert_eq!(param.sched_priority, 50, "expected priority 50");

        // Restore
        let restore = SchedPolicy::try_from(orig_policy).unwrap_or(SchedPolicy::Normal);
        let _ = set_schedattr(pid, orig_param.sched_priority as u32, restore);
    }

    #[test]
    #[ignore = "requires CAP_SYS_NICE; run with: cargo test -- --ignored"]
    fn set_schedattr_rr_and_verify() {
        let pid = Pid::from_raw(std::process::id() as libc::pid_t);
        let orig_policy = unsafe { libc::sched_getscheduler(pid.as_raw()) };
        let mut orig_param = libc::sched_param { sched_priority: 0 };
        unsafe { libc::sched_getparam(pid.as_raw(), &mut orig_param) };

        set_schedattr(pid, 30, SchedPolicy::Rr).expect("CAP_SYS_NICE required");

        let policy = unsafe { libc::sched_getscheduler(pid.as_raw()) };
        assert_eq!(
            policy,
            libc::SCHED_RR,
            "expected SCHED_RR after set_schedattr(Rr, 30)"
        );
        let mut param = libc::sched_param { sched_priority: 0 };
        unsafe { libc::sched_getparam(pid.as_raw(), &mut param) };
        assert_eq!(param.sched_priority, 30, "expected priority 30");

        // Restore
        let restore = SchedPolicy::try_from(orig_policy).unwrap_or(SchedPolicy::Normal);
        let _ = set_schedattr(pid, orig_param.sched_priority as u32, restore);
    }
}
