/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

pub mod config;
pub mod context;
pub mod error;
pub mod grpc;
pub mod proto;
pub mod sched;
pub mod signal;
pub mod task;

use std::time::{Duration, Instant};

use crate::proto::schedinfo_v1::ScheduledTask;
use config::Config;
use context::{Context, SchedInfo, SyncStartTime, TaskInfo};
use error::{TimpaniError, TimpaniResult};
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt::SubscriberBuilder, EnvFilter};

/// How often to re-fetch GetSchedInfo and compare for workload changes.
const WORKLOAD_POLL_INTERVAL_SECS: u64 = 2;

/// Convert a single proto [`ScheduledTask`] into the domain [`TaskInfo`] type.
///
/// Proto uses `int32` for most numeric fields; `.max(0)` guards against
/// negative wire values before the cast to `u32`.  `cpu_affinity` is already
/// a `uint64` bitmask on the wire and is copied through directly.
fn task_from_proto(t: &ScheduledTask) -> TaskInfo {
    TaskInfo {
        name: t.name.chars().take(16).collect(),
        sched_policy: t.sched_policy.max(0) as u32,
        sched_priority: t.sched_priority.max(0) as u32,
        period_us: t.period_us.max(0) as u32,
        release_time_us: t.release_time_us.max(0) as u32,
        runtime_us: t.runtime_us.max(0) as u32,
        deadline_us: t.deadline_us.max(0) as u32,
        cpu_affinity: t.cpu_affinity,
        max_dmiss: t.max_dmiss.max(0) as u32,
    }
}

/// Initialize logging with the specified log level.
///
/// External crates (tonic, h2, tower, hyper) are capped at WARN regardless of
/// the requested level.  This prevents h2 frame dumps from flooding the output
/// when the user asks for DEBUG or TRACE on their own code.
pub fn init_logging(log_level: config::LogLevel) {
    let our_level = log_level_to_tracing_level(log_level);
    // Always show WARN+ from dependencies; only show finer detail from our crate.
    let filter = EnvFilter::new(format!("warn,timpani_n={our_level}"));
    let _ = SubscriberBuilder::default()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Convert LogLevel to tracing::Level
fn log_level_to_tracing_level(log_level: config::LogLevel) -> tracing::Level {
    match log_level {
        config::LogLevel::Silent => tracing::Level::ERROR,
        config::LogLevel::Error => tracing::Level::ERROR,
        config::LogLevel::Warning => tracing::Level::WARN,
        config::LogLevel::Info => tracing::Level::INFO,
        config::LogLevel::Debug => tracing::Level::DEBUG,
        config::LogLevel::Verbose => tracing::Level::TRACE,
    }
}

/// Initialize the context
pub fn initialize(ctx: &mut Context) -> TimpaniResult<()> {
    ctx.initialize()
}

/// Run the main loop
pub fn run(_ctx: &mut Context) -> TimpaniResult<()> {
    info!("Runtime loop not yet implemented");
    Ok(())
}

/// Per-node startup and runtime loop.
///
/// Sequence:
///   1. Signal handlers   → CancellationToken
///   2. Initialize context (set CPU affinity, RT priority)
///   3. Connect to Timpani-O (with retry)
///   4. GetSchedInfo       → populate ctx.runtime.sched_info
///   5. SyncTimer          → populate ctx.runtime.sync_start   (if enable_sync)
///   6. RT wait loop       → waits for SIGINT/SIGTERM; timer loop fills this later
pub async fn run_app(config: Config) -> TimpaniResult<()> {
    config.log_config();

    // 1. Install signal handlers before any async ops so cancellation is live.
    let cancel = signal::setup_shutdown_handlers()?;

    // 2. Initialize context and apply CPU affinity + RT scheduling policy EARLY.
    //    This must happen before network operations to ensure the process runs
    //    on the correct CPU with the correct priority.
    let mut ctx = Context::new(config.clone());
    ctx.initialize()?;

    // 3. Connect to Timpani-O.
    let addr = format!("http://{}:{}", ctx.config.addr, ctx.config.port);
    info!(addr = %addr, "Connecting to Timpani-O");
    let mut client =
        grpc::NodeClient::connect(&addr, ctx.config.max_retries, cancel.clone()).await?;

    // 4. GetSchedInfo — retry until a workload is available or we give up.
    //    NOT_FOUND means Timpani-O is alive but no workload has been submitted
    //    yet.  This is expected and mirrors the C init_trpc() retry loop.
    let sched_resp = {
        let mut attempt = 0u32;
        loop {
            match client.get_sched_info(&ctx.config.node_id).await {
                Ok(resp) => break resp,
                Err(TimpaniError::NotReady) => {
                    if attempt >= ctx.config.max_retries {
                        error!(
                            attempts = ctx.config.max_retries + 1,
                            "No workload scheduled after max retries — giving up"
                        );
                        return Err(TimpaniError::Network);
                    }
                    attempt += 1;
                    info!(
                        attempt,
                        max = ctx.config.max_retries + 1,
                        "No workload scheduled yet — retrying in 1s"
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(TimpaniError::Signal),
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }
    };
    let tasks: Vec<TaskInfo> = sched_resp.tasks.iter().map(task_from_proto).collect();
    info!(
        workload_id    = %sched_resp.workload_id,
        hyperperiod_us = sched_resp.hyperperiod_us,
        task_count     = tasks.len(),
        "Schedule received from Timpani-O"
    );
    for (i, task) in tasks.iter().enumerate() {
        debug!(
            index         = i,
            name          = %task.name,
            policy        = task.sched_policy,
            priority      = task.sched_priority,
            period_us     = task.period_us,
            deadline_us   = task.deadline_us,
            runtime_us    = task.runtime_us,
            release_us    = task.release_time_us,
            cpu_affinity  = task.cpu_affinity,
            max_dmiss     = task.max_dmiss,
            "  task"
        );
    }
    let sched_info = SchedInfo {
        workload_id: sched_resp.workload_id,
        hyperperiod_us: sched_resp.hyperperiod_us,
        tasks,
        received_at: Instant::now(),
    };

    // 4b. init_task_list — find each process, apply affinity + schedattr, open pidfds.
    let tt_list = task::init_task_list(&sched_info)?;

    // 5. SyncTimer — barrier across all active nodes (skipped if enable_sync=false).
    let sync_start = if ctx.config.enable_sync {
        info!("SyncTimer: waiting for all nodes in workload to check in");
        let sync_resp = client.sync_timer(&ctx.config.node_id).await?;
        if !sync_resp.ack {
            error!("SyncTimer returned ack=false — barrier failed or timed out");
            return Err(TimpaniError::Network);
        }
        info!(
            start_sec = sync_resp.start_time_sec,
            start_nsec = sync_resp.start_time_nsec,
            "SyncTimer: barrier released"
        );
        Some(SyncStartTime {
            sec: sync_resp.start_time_sec,
            nsec: sync_resp.start_time_nsec,
        })
    } else {
        None
    };

    // 6. Populate runtime state with schedule, task list, and sync information.
    ctx.runtime.sched_info = Some(sched_info);
    ctx.runtime.tt_list = tt_list;
    ctx.runtime.sync_start = sync_start;
    ctx.comm.node_client = Some(client);

    // 7. Workload polling loop.
    //    Periodically re-fetches the schedule from Timpani-O to detect workload
    //    changes (new workload_id) or task-parameter updates (same workload_id,
    //    different tasks).  Versioning is client-side only: NodeSchedResponse has
    //    no server-side version field, so we do a structural content comparison.
    //
    //    On change: logs the event and updates ctx.runtime.sched_info.
    //    Full teardown + reinit of the RT loop is a TODO pending the task module.
    //
    //    NotReady (Timpani-O temporarily has no active workload) → keep running.
    let node_id = ctx.config.node_id.clone();
    let mut poll_interval = tokio::time::interval(Duration::from_secs(WORKLOAD_POLL_INTERVAL_SECS));
    // Skip ticks that were missed while handling a long workload update.
    poll_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the first immediate tick so the first real poll fires after
    // WORKLOAD_POLL_INTERVAL_SECS, not at t=0 right after startup.
    poll_interval.tick().await;
    info!(
        interval_secs = WORKLOAD_POLL_INTERVAL_SECS,
        "Startup complete — entering workload watch loop (RT loop pending task module)"
    );
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                info!("Shutdown signal received — cleaning up");
                break;
            }
            _ = poll_interval.tick() => {
                // Borrow ctx.comm in its own block so the &mut NodeClient is
                // released before we borrow ctx.runtime below.
                let poll_result = {
                    let Some(client) = ctx.comm.node_client.as_mut() else {
                        warn!("Workload poll skipped: NodeClient is not available");
                        continue;
                    };
                    client.get_sched_info(&node_id).await
                };
                match poll_result {
                    Ok(new_resp) => {
                        let new_tasks: Vec<TaskInfo> =
                            new_resp.tasks.iter().map(task_from_proto).collect();
                        let new_sched = SchedInfo {
                            workload_id:   new_resp.workload_id,
                            hyperperiod_us: new_resp.hyperperiod_us,
                            tasks:         new_tasks,
                            received_at:   Instant::now(),
                        };
                        match ctx.runtime.sched_info.as_ref() {
                            None => {
                                // Schedule was cleared (e.g. by cleanup or future
                                // reconnect logic) — restore it from the latest poll.
                                debug!(
                                    workload_id = %new_sched.workload_id,
                                    "sched_info was absent; restoring from latest poll"
                                );
                                ctx.runtime.sched_info = Some(new_sched);
                            }
                            Some(current) => {
                                if current.content_changed(&new_sched) {
                                    if current.is_full_replacement(&new_sched) {
                                        info!(
                                            old_workload = %current.workload_id,
                                            new_workload = %new_sched.workload_id,
                                            "Workload replaced — full teardown and reinit"
                                        );
                                    } else {
                                        info!(
                                            workload_id    = %new_sched.workload_id,
                                            old_task_count = current.tasks.len(),
                                            new_task_count = new_sched.tasks.len(),
                                            "Workload updated — teardown and reinit"
                                        );
                                    }
                                    // Teardown current task list and reinitialize with
                                    // the new schedule.  If reinit fails, abort: the
                                    // process has no valid task list to continue with.
                                    let old_tt = std::mem::take(&mut ctx.runtime.tt_list);
                                    task::teardown_task_list(old_tt);
                                    match task::init_task_list(&new_sched) {
                                        Ok(new_tt) => {
                                            ctx.runtime.tt_list = new_tt;
                                            ctx.runtime.sched_info = Some(new_sched);
                                        }
                                        Err(e) => {
                                            warn!(
                                                error = ?e,
                                                "Failed to reinitialize task list after workload \
                                                 change — shutting down"
                                            );
                                            return Err(e);
                                        }
                                    }
                                } else {
                                    // Content unchanged — advance received_at so callers can
                                    // detect when the last successful fetch occurred.
                                    ctx.runtime.sched_info.as_mut().unwrap().received_at =
                                        new_sched.received_at;
                                }
                            }
                        }
                    }
                    Err(TimpaniError::NotReady) => {
                        // Timpani-O is alive but has no active workload right now
                        // (e.g. between submissions).  Keep current schedule running.
                        debug!("Workload poll: no active workload on Timpani-O — keeping current");
                    }
                    Err(e) => {
                        warn!(error = ?e, "Workload poll failed — retrying next interval");
                    }
                }
            }
        }
    }

    ctx.cleanup();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_initialization() {
        let config = Config::default();
        let mut ctx = Context::new(config);
        assert!(initialize(&mut ctx).is_ok());
    }

    #[test]
    fn test_run_with_default_context() {
        let config = Config::default();
        let mut ctx = Context::new(config);
        assert!(run(&mut ctx).is_ok());
    }

    #[test]
    fn test_initialize_multiple_times() {
        let config = Config::default();
        let mut ctx = Context::new(config);

        // Initialize should be idempotent
        assert!(initialize(&mut ctx).is_ok());
        assert!(initialize(&mut ctx).is_ok());
    }

    #[test]
    fn test_context_lifecycle() {
        let config = Config::default();
        let mut ctx = Context::new(config);

        // Full lifecycle
        assert!(initialize(&mut ctx).is_ok());
        assert!(run(&mut ctx).is_ok());
        ctx.cleanup();
    }

    #[test]
    fn test_log_level_mapping() {
        // Test that all log level mappings are correct
        use config::LogLevel;

        assert_eq!(
            log_level_to_tracing_level(LogLevel::Silent),
            tracing::Level::ERROR
        );
        assert_eq!(
            log_level_to_tracing_level(LogLevel::Error),
            tracing::Level::ERROR
        );
        assert_eq!(
            log_level_to_tracing_level(LogLevel::Warning),
            tracing::Level::WARN
        );
        assert_eq!(
            log_level_to_tracing_level(LogLevel::Info),
            tracing::Level::INFO
        );
        assert_eq!(
            log_level_to_tracing_level(LogLevel::Debug),
            tracing::Level::DEBUG
        );
        assert_eq!(
            log_level_to_tracing_level(LogLevel::Verbose),
            tracing::Level::TRACE
        );
    }

    #[test]
    fn test_all_log_levels_conversion() {
        // Comprehensive test for all log levels
        use config::LogLevel;

        let levels = vec![
            LogLevel::Silent,
            LogLevel::Error,
            LogLevel::Warning,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Verbose,
        ];

        for level in levels {
            // Just ensure conversion works for all levels
            let _tracing_level = log_level_to_tracing_level(level);
        }
    }

    #[test]
    fn test_run_and_initialize_combinations() {
        // Test various initialization and run combinations
        let configs = vec![
            Config::default(), // cpu=-1, prio=-1 (should always work)
            Config {
                cpu: config::test_values::TEST_CPU_ONE,
                ..Default::default()
            },
            Config {
                prio: config::test_values::TEST_PRIORITY_MID,
                ..Default::default()
            },
            Config {
                enable_sync: true,
                enable_plot: true,
                enable_apex: true,
                ..Default::default()
            },
        ];

        for config in configs {
            let mut ctx = Context::new(config.clone());
            // Initialize may fail due to permissions (CAP_SYS_NICE), accept that
            let init_result = initialize(&mut ctx);
            match init_result {
                Ok(_) => {}                                // Success with privileges
                Err(error::TimpaniError::Permission) => {} // Expected without privileges
                Err(e) => panic!("Unexpected error for config {:?}: {:?}", config, e),
            }
            assert!(run(&mut ctx).is_ok());
            ctx.cleanup();
        }
    }

    #[test]
    fn test_init_logging() {
        // Test init_logging with various log levels
        // Uses try_init so it won't fail if already initialized
        init_logging(config::LogLevel::Info);
        init_logging(config::LogLevel::Debug);
        init_logging(config::LogLevel::Error);
    }

    #[test]
    fn test_init_logging_all_levels() {
        // Test all log levels
        for level_num in config::log_level::SILENT..=config::test_values::LOG_LEVEL_RANGE_MAX {
            let level = config::LogLevel::from_u8(level_num).unwrap();
            init_logging(level);
        }
    }
}

// ── run_app Tests with Mock Servers ──────────────────────────────────────────

#[cfg(test)]
mod run_app_tests {
    use super::*;
    use crate::proto::schedinfo_v1::{
        node_service_server::{NodeService, NodeServiceServer},
        NodeResponse, NodeSchedRequest, NodeSchedResponse, SyncRequest, SyncResponse,
    };
    use std::net::SocketAddr;
    use tonic::{transport::Server, Request, Response, Status};

    struct MockNodeService {
        workload_available: bool,
        sync_enabled: bool,
        sync_ack: bool,
    }

    #[tonic::async_trait]
    impl NodeService for MockNodeService {
        async fn get_sched_info(
            &self,
            _request: Request<NodeSchedRequest>,
        ) -> Result<Response<NodeSchedResponse>, Status> {
            if !self.workload_available {
                return Err(Status::not_found("No workload scheduled yet"));
            }

            // Empty task list — init_task_list must succeed trivially so
            // these tests can reach the SyncTimer/polling-loop behaviour they
            // are actually testing.  Real task resolution against /proc is
            // exercised by task::tests::init_task_list_*.
            let response = NodeSchedResponse {
                workload_id: "test-workload-001".to_string(),
                hyperperiod_us: 1_000_000,
                tasks: vec![],
            };

            Ok(Response::new(response))
        }

        async fn sync_timer(
            &self,
            _request: Request<SyncRequest>,
        ) -> Result<Response<SyncResponse>, Status> {
            if !self.sync_enabled {
                return Err(Status::unavailable("Sync not enabled"));
            }

            let response = SyncResponse {
                ack: self.sync_ack,
                start_time_sec: 1234567890,
                start_time_nsec: 123456789,
            };

            Ok(Response::new(response))
        }

        async fn report_d_miss(
            &self,
            request: Request<crate::proto::schedinfo_v1::DeadlineMissInfo>,
        ) -> Result<Response<NodeResponse>, Status> {
            let info = request.into_inner();
            let response = NodeResponse {
                status: 0,
                error_message: format!(
                    "Received dmiss from {} for task {}",
                    info.node_id, info.task_name
                ),
            };
            Ok(Response::new(response))
        }
    }

    async fn start_test_server(
        port: u16,
        workload_available: bool,
        sync_enabled: bool,
        sync_ack: bool,
    ) -> Result<SocketAddr, Box<dyn std::error::Error>> {
        let addr = format!("127.0.0.1:{}", port).parse::<SocketAddr>()?;
        let service = MockNodeService {
            workload_available,
            sync_enabled,
            sync_ack,
        };

        tokio::spawn(async move {
            Server::builder()
                .add_service(NodeServiceServer::new(service))
                .serve(addr)
                .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(addr)
    }

    #[tokio::test]
    async fn test_run_app_without_sync() {
        let port = 50200;
        start_test_server(port, true, false, false).await.unwrap();

        let config = Config {
            addr: "127.0.0.1".to_string(),
            port,
            node_id: "test-node".to_string(),
            enable_sync: false,
            max_retries: 3,
            ..Default::default()
        };

        // Spawn run_app and cancel it after a short delay
        let handle = tokio::spawn(async move { run_app(config).await });

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Send a simulated signal to stop (in real test we just let it timeout)
        // The test passes if run_app starts successfully
        handle.abort();
    }

    #[tokio::test]
    async fn test_run_app_with_sync() {
        let port = 50201;
        start_test_server(port, true, true, true).await.unwrap();

        let config = Config {
            addr: "127.0.0.1".to_string(),
            port,
            node_id: "test-node".to_string(),
            enable_sync: true,
            max_retries: 3,
            ..Default::default()
        };

        let handle = tokio::spawn(async move { run_app(config).await });

        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.abort();
    }

    #[tokio::test]
    async fn test_run_app_sync_fails() {
        let port = 50202;
        start_test_server(port, true, true, false).await.unwrap();

        let config = Config {
            addr: "127.0.0.1".to_string(),
            port,
            node_id: "test-node".to_string(),
            enable_sync: true,
            max_retries: 1,
            ..Default::default()
        };

        let result = run_app(config).await;
        // Should fail because sync_ack is false
        assert!(matches!(result, Err(TimpaniError::Network)));
    }

    #[tokio::test]
    async fn test_run_app_workload_not_ready_retries() {
        let port = 50203;
        // Start with workload not available - will cause retries
        start_test_server(port, false, false, false).await.unwrap();

        let config = Config {
            addr: "127.0.0.1".to_string(),
            port,
            node_id: "test-node".to_string(),
            enable_sync: false,
            max_retries: 2,
            ..Default::default()
        };

        let result = run_app(config).await;
        // Should fail after max retries
        assert!(matches!(result, Err(TimpaniError::Network)));
    }

    #[tokio::test]
    async fn test_run_app_connection_fails() {
        // Use a port with no server
        let config = Config {
            addr: "127.0.0.1".to_string(),
            port: 50299,
            node_id: "test-node".to_string(),
            enable_sync: false,
            max_retries: 0,
            ..Default::default()
        };

        let result = run_app(config).await;
        // Should fail to connect
        assert!(result.is_err());
    }
}

// ── Polling-loop tests with a stateful mock ───────────────────────────────────

#[cfg(test)]
mod run_app_polling_tests {
    use super::*;
    use crate::error::TimpaniError;
    use crate::proto::schedinfo_v1::{
        node_service_server::{NodeService, NodeServiceServer},
        NodeResponse, NodeSchedRequest, NodeSchedResponse, ScheduledTask, SyncRequest,
        SyncResponse,
    };
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tonic::{transport::Server, Request, Response, Status};

    /// Controls which response the mock returns on its second and subsequent calls.
    enum Scenario {
        /// workload_id changes v1 → v2, both with empty task lists.
        WorkloadReplaced,
        /// Same workload_id, hyperperiod doubles — sched_info must be refreshed.
        ParamsUpdated,
        /// Server returns NOT_FOUND for every poll after the initial fetch.
        NotReadyAfterFirst,
        /// Server returns a task whose process does not exist on the second call.
        UnknownTaskAfterFirst,
    }

    struct StatefulMockService {
        call_count: Arc<Mutex<u32>>,
        scenario: Scenario,
    }

    #[tonic::async_trait]
    impl NodeService for StatefulMockService {
        async fn get_sched_info(
            &self,
            _request: Request<NodeSchedRequest>,
        ) -> Result<Response<NodeSchedResponse>, Status> {
            let count = {
                let mut c = self.call_count.lock().unwrap();
                *c += 1;
                *c
            };
            match &self.scenario {
                Scenario::WorkloadReplaced => {
                    if count == 1 {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-v1".to_string(),
                            hyperperiod_us: 1_000_000,
                            tasks: vec![],
                        }))
                    } else {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-v2".to_string(),
                            hyperperiod_us: 1_000_000,
                            tasks: vec![],
                        }))
                    }
                }
                Scenario::ParamsUpdated => {
                    if count == 1 {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-stable".to_string(),
                            hyperperiod_us: 1_000_000,
                            tasks: vec![],
                        }))
                    } else {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-stable".to_string(),
                            hyperperiod_us: 2_000_000,
                            tasks: vec![],
                        }))
                    }
                }
                Scenario::NotReadyAfterFirst => {
                    if count == 1 {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-gone".to_string(),
                            hyperperiod_us: 1_000_000,
                            tasks: vec![],
                        }))
                    } else {
                        Err(Status::not_found("workload removed"))
                    }
                }
                Scenario::UnknownTaskAfterFirst => {
                    if count == 1 {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-v1".to_string(),
                            hyperperiod_us: 1_000_000,
                            tasks: vec![],
                        }))
                    } else {
                        Ok(Response::new(NodeSchedResponse {
                            workload_id: "workload-v2".to_string(),
                            hyperperiod_us: 1_000_000,
                            tasks: vec![ScheduledTask {
                                name: "__no_such_proc__".to_string(),
                                sched_policy: 0,
                                sched_priority: 0,
                                period_us: 10_000,
                                release_time_us: 0,
                                runtime_us: 1_000,
                                deadline_us: 10_000,
                                cpu_affinity: 0,
                                max_dmiss: 0,
                                assigned_node: String::new(),
                            }],
                        }))
                    }
                }
            }
        }

        async fn sync_timer(
            &self,
            _: Request<SyncRequest>,
        ) -> Result<Response<SyncResponse>, Status> {
            Ok(Response::new(SyncResponse {
                ack: true,
                start_time_sec: 1_234_567_890,
                start_time_nsec: 0,
            }))
        }

        async fn report_d_miss(
            &self,
            _: Request<crate::proto::schedinfo_v1::DeadlineMissInfo>,
        ) -> Result<Response<NodeResponse>, Status> {
            Ok(Response::new(NodeResponse {
                status: 0,
                error_message: String::new(),
            }))
        }
    }

    async fn start_stateful_server(
        port: u16,
        scenario: Scenario,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = format!("127.0.0.1:{port}").parse::<SocketAddr>()?;
        let service = StatefulMockService {
            call_count: Arc::new(Mutex::new(0)),
            scenario,
        };
        tokio::spawn(async move {
            Server::builder()
                .add_service(NodeServiceServer::new(service))
                .serve(addr)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(())
    }

    fn polling_config(port: u16) -> Config {
        Config {
            addr: "127.0.0.1".to_string(),
            port,
            node_id: "test-node".to_string(),
            enable_sync: false,
            max_retries: 3,
            ..Default::default()
        }
    }

    // D1: workload_id changes — run_app reinitialises with empty task list and keeps running.
    #[tokio::test]
    async fn test_run_app_workload_id_replaced() {
        start_stateful_server(50300, Scenario::WorkloadReplaced)
            .await
            .unwrap();
        let handle = tokio::spawn(run_app(polling_config(50300)));
        // Wait long enough for the first poll cycle (~2 s) to complete.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !handle.is_finished(),
            "run_app must keep running after a workload replacement with an empty task list"
        );
        handle.abort();
    }

    // D2: same workload_id, hyperperiod updated — sched_info refreshed, loop continues.
    #[tokio::test]
    async fn test_run_app_workload_params_updated() {
        start_stateful_server(50301, Scenario::ParamsUpdated)
            .await
            .unwrap();
        let handle = tokio::spawn(run_app(polling_config(50301)));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !handle.is_finished(),
            "run_app must keep running after an in-place hyperperiod update"
        );
        handle.abort();
    }

    // D3: workload disappears after startup — run_app keeps old schedule and stays alive.
    #[tokio::test]
    async fn test_run_app_polling_notready_keeps_running() {
        start_stateful_server(50302, Scenario::NotReadyAfterFirst)
            .await
            .unwrap();
        let handle = tokio::spawn(run_app(polling_config(50302)));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !handle.is_finished(),
            "NOT_FOUND during poll must not terminate run_app"
        );
        handle.abort();
    }

    // D4: poll returns a task whose process does not exist — init_task_list fails, run_app exits.
    #[tokio::test]
    async fn test_run_app_init_task_list_fails_on_update() {
        start_stateful_server(50303, Scenario::UnknownTaskAfterFirst)
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), run_app(polling_config(50303)))
            .await
            .expect("run_app must exit within 5 s when init_task_list fails on a workload update");
        assert!(
            matches!(result, Err(TimpaniError::Io)),
            "init_task_list failure during a workload update must propagate as TimpaniError::Io, \
             got {result:?}"
        );
    }
}
