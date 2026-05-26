/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

use crate::config::Config;
use crate::grpc::NodeClient;
use crate::sched::{set_affinity, set_schedattr, SchedPolicy};
use nix::unistd::Pid;
use tracing::{info, warn};

/// Scheduling information received from Timpani-O at startup via GetSchedInfo.
///
/// This is a domain type (no proto dependency).  The full task list lives here
/// temporarily until the task module is implemented and owns it.
#[derive(Debug)]
pub struct SchedInfo {
    /// Workload identifier string from Timpani-O.
    pub workload_id: String,
    /// Hyperperiod in microseconds.
    pub hyperperiod_us: u64,
    /// Number of tasks assigned to this node.
    pub task_count: usize,
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
    // TODO: Add fields as we port more modules:
    // - tt_list (time trigger task list — task module)
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
}
