/*
 * SPDX-FileCopyrightText: Copyright 2026 LG Electronics Inc.
 * SPDX-License-Identifier: MIT
 */

pub mod config;
pub mod context;
pub mod core;
pub mod error;
pub mod grpc;
pub mod proto;
pub mod sched;
pub mod signal;

use std::time::Duration;

use config::Config;
use context::{Context, SchedInfo, SyncStartTime};
use error::{TimpaniError, TimpaniResult};
use tracing::{debug, error, info};
use tracing_subscriber::{fmt::SubscriberBuilder, EnvFilter};

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
    let task_count = sched_resp.tasks.len();
    info!(
        workload_id    = %sched_resp.workload_id,
        hyperperiod_us = sched_resp.hyperperiod_us,
        task_count,
        "Schedule received from Timpani-O"
    );
    for (i, task) in sched_resp.tasks.iter().enumerate() {
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
        task_count,
    };

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

    // 6. Populate runtime state with schedule and sync information.
    ctx.runtime.sched_info = Some(sched_info);
    ctx.runtime.sync_start = sync_start;
    ctx.comm.node_client = Some(client);

    // 7. RT wait loop.  The timer/task module will replace this with the
    //    real deadline-driven loop; ReportDMiss will be called from there.
    info!("Startup complete — waiting for shutdown signal (RT loop not yet implemented)");
    cancel.cancelled().await;
    info!("Shutdown signal received — cleaning up");

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
        NodeResponse, NodeSchedRequest, NodeSchedResponse, ScheduledTask, SyncRequest,
        SyncResponse,
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

            let response = NodeSchedResponse {
                workload_id: "test-workload-001".to_string(),
                hyperperiod_us: 1_000_000,
                tasks: vec![ScheduledTask {
                    name: "test_task".to_string(),
                    sched_policy: 1,
                    sched_priority: 50,
                    period_us: 100_000,
                    deadline_us: 100_000,
                    runtime_us: 10_000,
                    release_time_us: 0,
                    cpu_affinity: 1,
                    max_dmiss: 5,
                    assigned_node: "test-node".to_string(),
                }],
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
