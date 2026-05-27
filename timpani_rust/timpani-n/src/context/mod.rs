/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

use std::time::Instant;

use crate::config::Config;
use crate::grpc::NodeClient;
use crate::sched::{set_affinity, set_schedattr, SchedPolicy};
use crate::task::TimeTrigger;
use nix::unistd::Pid;
use tracing::{info, warn};

/// Static scheduling parameters for one task, received from Timpani-O.
///
/// Pure domain type — no proto dependency, no runtime state (pid, pidfd).
/// Those are added by the task module during `init_task_list`.
///
/// Mirrors `struct task_info` from schedinfo.h, minus the runtime fields.
/// Will move to `task/mod.rs` when that module is implemented.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskInfo {
    /// Task name.  Certain conversion helpers (e.g. `task_from_proto`) may truncate
    /// this to at most 16 chars (`TINFO_NAME_MAX`), but `TaskInfo` itself does not
    /// enforce that limit.
    pub name: String,
    /// Linux scheduling policy (0 = NORMAL, 1 = FIFO, 2 = RR).
    pub sched_policy: u32,
    /// Linux real-time scheduling priority (0 for NORMAL, 1–99 for FIFO/RR).
    pub sched_priority: u32,
    /// Task period in microseconds.
    pub period_us: u32,
    /// Release time offset within the hyperperiod, in microseconds.
    pub release_time_us: u32,
    /// Worst-case execution time budget in microseconds.
    pub runtime_us: u32,
    /// Relative deadline in microseconds.
    pub deadline_us: u32,
    /// CPU affinity bitmask (same bit layout as Linux cpu_set_t).
    pub cpu_affinity: u64,
    /// Maximum allowable deadline misses before a fault is reported.
    pub max_dmiss: u32,
}

/// Scheduling information received from Timpani-O via GetSchedInfo.
///
/// This is a domain type (no proto dependency).  Versioned client-side via
/// `received_at`: Timpani-N polls GetSchedInfo periodically and compares
/// content (excluding `received_at`) to detect workload changes.
#[derive(Debug)]
pub struct SchedInfo {
    /// Workload identifier.  A change here means full workload replacement.
    pub workload_id: String,
    /// Hyperperiod in microseconds.
    pub hyperperiod_us: u64,
    /// Tasks assigned to this node.
    pub tasks: Vec<TaskInfo>,
    /// Wall-clock time when this schedule version was fetched from Timpani-O.
    /// Used for logging and staleness tracking only.  Excluded from PartialEq.
    pub received_at: Instant,
}

impl PartialEq for SchedInfo {
    /// Content equality — `received_at` is intentionally excluded.
    fn eq(&self, other: &Self) -> bool {
        self.workload_id == other.workload_id
            && self.hyperperiod_us == other.hyperperiod_us
            && self.tasks == other.tasks
    }
}

impl SchedInfo {
    /// Returns `true` if the schedule content differs from `other`.
    ///
    /// `received_at` is not considered — only `workload_id`, `hyperperiod_us`,
    /// and `tasks` participate in the comparison.
    pub fn content_changed(&self, other: &SchedInfo) -> bool {
        self != other
    }

    /// Returns `true` if `other` belongs to a completely new workload
    /// (different `workload_id`) rather than an in-place update.
    pub fn is_full_replacement(&self, other: &SchedInfo) -> bool {
        self.workload_id != other.workload_id
    }

    /// Convenience accessor: number of tasks in this schedule.
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

/// Absolute start time returned by SyncTimer when the barrier releases.
///
/// Expressed as a CLOCK_REALTIME value — the timer module uses this to
/// calculate when each task's first deadline fires.
#[derive(Debug, Clone, Copy)]
pub struct SyncStartTime {
    pub sec: i64,
    pub nsec: i32,
}

/// Runtime state structure
/// Maps to context.runtime from C implementation
#[derive(Debug, Default)]
pub struct RuntimeState {
    /// Shutdown request flag
    pub shutdown_requested: bool,
    /// Schedule received from Timpani-O at startup.  None until GetSchedInfo succeeds.
    pub sched_info: Option<SchedInfo>,
    /// Barrier start time from SyncTimer.  None if enable_sync=false or sync not yet done.
    pub sync_start: Option<SyncStartTime>,
    /// Runtime task list.  Empty until init_task_list() succeeds after GetSchedInfo.
    /// Rebuilt on every workload change detected in the polling loop.
    pub tt_list: Vec<TimeTrigger>,
    // TODO: Add fields as we port more modules:
    // - apex_list (Apex.OS task list — apex module)
}

/// Communication state structure
/// Maps to context.comm from C implementation
#[derive(Debug, Default)]
pub struct CommState {
    /// Live gRPC connection to Timpani-O.  None until NodeClient::connect succeeds.
    pub node_client: Option<NodeClient>,
    // TODO: Add fields as we port more modules:
    // - apex_fd (Apex.OS Monitor Socket FD)
}

/// Hyperperiod manager structure
/// Maps to context.hp_manager from C implementation
#[derive(Debug, Default)]
pub struct HyperperiodManager {
    // TODO: Add fields as we port hyperperiod module:
    // - hyperperiod_us
    // - current_cycle
    // - workload_id
    // - etc.
}

/// Main context structure for Timpani-N
/// Maps to the C struct context
/// Centralizes all state and configuration
#[derive(Debug)]
pub struct Context {
    /// System configuration
    pub config: Config,

    /// Runtime state (dynamic state during execution)
    pub runtime: RuntimeState,

    /// Communication state (D-Bus, event loop)
    pub comm: CommState,

    /// Hyperperiod manager
    pub hp_manager: HyperperiodManager,
}

impl Context {
    /// Create a new context with the given configuration
    pub fn new(config: Config) -> Self {
        Context {
            config,
            runtime: RuntimeState::default(),
            comm: CommState::default(),
            hp_manager: HyperperiodManager::default(),
        }
    }

    /// Initialize the context
    ///
    /// This applies system-level configuration (affinity, scheduling policy)
    /// to the current process. Future work includes BPF setup, task list
    /// initialization, and Apex.OS monitor integration.
    pub fn initialize(&mut self) -> crate::error::TimpaniResult<()> {
        let pid = Pid::from_raw(std::process::id() as i32);

        // Apply CPU affinity if specified (cpu >= 0 means pin to specific CPU)
        if self.config.cpu >= 0 {
            info!("Setting CPU affinity to CPU {}", self.config.cpu);
            set_affinity(pid, self.config.cpu as u32)?;
        } else {
            warn!("CPU affinity not set (cpu=-1 means no pinning)");
        }

        // Apply scheduling policy and priority if specified (prio >= 0)
        if self.config.prio >= 0 {
            // Determine policy based on priority:
            // - prio 1-99: SCHED_FIFO (real-time)
            // - prio 0: SCHED_OTHER (normal)
            let policy = if self.config.prio > 0 && self.config.prio <= 99 {
                SchedPolicy::Fifo
            } else {
                SchedPolicy::Normal
            };

            info!(
                "Setting scheduling policy to {:?} with priority {}",
                policy, self.config.prio
            );
            set_schedattr(pid, self.config.prio as u32, policy)?;
        } else {
            warn!("Scheduling policy not modified (prio=-1 means default)");
        }

        // Calibrate BPF time offset for timestamp conversion
        info!("Calibrating BPF time offset");
        crate::core::calibrate_time_offset()?;

        // TODO: Add additional initialization logic as we port more modules:
        // - init_task_list
        // - apex_monitor_init

        Ok(())
    }

    /// Cleanup resources (placeholder for future cleanup logic)
    pub fn cleanup(&mut self) {
        // TODO: Add cleanup logic as we port more modules:
        // - cleanup time triggers
        // - cleanup BPF resources
        // - cleanup network connections
        // - cleanup hyperperiod manager
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_context_creation() {
        let config = Config::default();
        let ctx = Context::new(config);
        assert!(!ctx.runtime.shutdown_requested);
    }

    #[test]
    fn test_runtime_default() {
        let runtime = RuntimeState::default();
        assert!(!runtime.shutdown_requested);
    }

    #[test]
    fn test_context_initialization() {
        let config = Config::default();
        let mut ctx = Context::new(config);
        assert!(ctx.initialize().is_ok());
    }

    #[test]
    fn test_context_cleanup() {
        let config = Config::default();
        let mut ctx = Context::new(config);
        ctx.cleanup(); // Should not panic
    }

    #[test]
    fn test_context_initialization_with_defaults() {
        // Default config has cpu=0, prio=0 which should skip affinity/sched setup
        let config = Config::default();
        let mut ctx = Context::new(config);
        // Should succeed even without setting affinity (cpu=0 means skip)
        assert!(ctx.initialize().is_ok());
    }

    #[test]
    #[ignore] // Requires CAP_SYS_NICE for RT priority
    fn test_context_initialization_with_rt_priority() {
        let config = Config {
            cpu: 0, // Skip affinity
            prio: 50,
            ..Default::default()
        };
        let mut ctx = Context::new(config);
        // May fail without privileges
        let _ = ctx.initialize();
    }

    #[test]
    fn test_context_initialization_with_cpu_affinity() {
        let config = Config {
            cpu: 1,  // Pin to CPU 1
            prio: 0, // Skip scheduling
            ..Default::default()
        };
        let mut ctx = Context::new(config);
        // May fail without privileges but should attempt it
        let _ = ctx.initialize();
    }

    #[test]
    #[ignore] // Requires CAP_SYS_NICE
    fn test_context_full_initialization() {
        let config = Config {
            cpu: 1,
            prio: 85,
            ..Default::default()
        };
        let mut ctx = Context::new(config);
        // Will likely fail without privileges
        let _ = ctx.initialize();
    }

    #[test]
    fn test_comm_state_default() {
        let comm = CommState::default();
        // Just ensure it constructs without issues
        let _ = format!("{:?}", comm);
    }

    #[test]
    fn test_hyperperiod_manager_default() {
        let hp_mgr = HyperperiodManager::default();
        // Just ensure it constructs without issues
        let _ = format!("{:?}", hp_mgr);
    }

    #[test]
    fn test_context_with_custom_config() {
        let mut config = Config::default();
        config.cpu = crate::config::test_values::TEST_CPU_AFFINITY;
        config.prio = crate::config::test_values::TEST_PRIORITY;
        config.node_id = crate::config::test_values::TEST_NODE_ID_SHORT.to_string();

        let mut ctx = Context::new(config);
        // May fail without CAP_SYS_NICE permission, but shouldn't panic
        let result = ctx.initialize();
        match result {
            Ok(_) => {}                                       // Success with privileges
            Err(crate::error::TimpaniError::Permission) => {} // Expected without privileges
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
        ctx.cleanup();
    }

    fn make_task(name: &str, period_us: u32) -> TaskInfo {
        TaskInfo {
            name: name.chars().take(16).collect(),
            sched_policy: 2,
            sched_priority: 50,
            period_us,
            release_time_us: 0,
            runtime_us: 1000,
            deadline_us: period_us,
            cpu_affinity: 0x1,
            max_dmiss: 0,
        }
    }

    fn make_sched(workload_id: &str, tasks: Vec<TaskInfo>) -> SchedInfo {
        SchedInfo {
            workload_id: workload_id.to_string(),
            hyperperiod_us: 100_000,
            tasks,
            received_at: Instant::now(),
        }
    }

    #[test]
    fn test_sched_info_equal_content_different_received_at() {
        let task = make_task("task_a", 10_000);
        let a = make_sched("wl-1", vec![task.clone()]);
        // Simulate a later fetch: same content, different received_at.
        let b = SchedInfo {
            workload_id: a.workload_id.clone(),
            hyperperiod_us: a.hyperperiod_us,
            tasks: a.tasks.clone(),
            received_at: Instant::now(),
        };
        assert_eq!(a, b, "Same content => equal regardless of received_at");
        assert!(!a.content_changed(&b), "No content change expected");
    }

    #[test]
    fn test_sched_info_content_changed_task_param() {
        let a = make_sched("wl-1", vec![make_task("task_a", 10_000)]);
        // Same workload_id, but period_us changed.
        let b = make_sched("wl-1", vec![make_task("task_a", 20_000)]);
        assert_ne!(a, b);
        assert!(a.content_changed(&b), "period_us change must be detected");
        assert!(
            !a.is_full_replacement(&b),
            "Same workload_id is not a replacement"
        );
    }

    #[test]
    fn test_sched_info_full_replacement() {
        let a = make_sched("wl-1", vec![make_task("task_a", 10_000)]);
        let b = make_sched("wl-2", vec![make_task("task_b", 10_000)]);
        assert!(a.content_changed(&b));
        assert!(
            a.is_full_replacement(&b),
            "Different workload_id is a full replacement"
        );
    }

    #[test]
    fn test_sched_info_task_count() {
        let a = make_sched(
            "wl-1",
            vec![make_task("t1", 5_000), make_task("t2", 10_000)],
        );
        assert_eq!(a.task_count(), 2);
    }
}
