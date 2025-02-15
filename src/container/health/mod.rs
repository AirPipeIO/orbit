// src/container/health/mod.rs
use crate::container::ContainerRuntime;
use anyhow::Result;
use dashmap::DashMap;
use serde::Serialize;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;

pub use self::config::HealthCheckConfig;
use super::RUNTIME;
mod config;

pub static CONTAINER_HEALTH: OnceLock<DashMap<String, ContainerHealthState>> = OnceLock::new();

use std::sync::atomic::{AtomicU64, Ordering};

// Just use DashMap for the cache
pub static HEALTH_CACHE: OnceLock<DashMap<String, (ContainerHealthState, AtomicU64)>> =
    OnceLock::new();
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
pub enum HealthState {
    Starting,
    Healthy,
    Unhealthy,
    CrashLoopBackOff,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerHealthState {
    pub state: HealthState,
    pub last_state: Option<HealthState>,
    pub last_transition: SystemTime,
    pub restart_count: u32,
    pub last_restart: Option<SystemTime>,
    pub failure_count: u32,
    pub last_failure: Option<SystemTime>,
    pub message: Option<String>,
}

impl Default for ContainerHealthState {
    fn default() -> Self {
        Self {
            state: HealthState::Starting,
            last_state: None,
            last_transition: SystemTime::now(),
            restart_count: 0,
            last_restart: None,
            failure_count: 0,
            last_failure: None,
            message: None,
        }
    }
}

impl ContainerHealthState {
    pub fn transition_to(&mut self, new_state: HealthState, message: Option<String>) {
        self.last_state = Some(self.state.clone());
        self.state = new_state;
        self.last_transition = SystemTime::now();
        self.message = message;
    }

    fn record_failure(&mut self) {
        self.failure_count += 1;
        self.last_failure = Some(SystemTime::now());
    }

    // fn record_restart(&mut self) {
    //     self.restart_count += 1;
    //     self.last_restart = Some(SystemTime::now());
    // }
}

pub async fn initialize_health_monitoring(
    service_name: &str,
    container_name: &str,
    config: Option<HealthCheckConfig>,
) -> Result<()> {
    let health_store = CONTAINER_HEALTH.get_or_init(DashMap::new);
    let health_cache = HEALTH_CACHE.get_or_init(DashMap::new);
    let config = config.unwrap_or_default();

    let runtime = RUNTIME.get().expect("Runtime not initialized").clone();

    // Initialize health state
    let initial_state = ContainerHealthState::default();
    health_store.insert(container_name.to_string(), initial_state.clone());

    // Initialize cache
    let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
    health_cache.insert(
        container_name.to_string(),
        (initial_state, AtomicU64::new(generation)),
    );

    // Spawn monitoring task
    tokio::spawn(monitor_container_health(
        service_name.to_string(),
        container_name.to_string(),
        config,
        runtime,
    ));

    Ok(())
}

async fn check_tcp_health(addr: &str, port: u16, timeout: Duration) -> bool {
    match tokio::time::timeout(timeout, TcpStream::connect(format!("{}:{}", addr, port))).await {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

async fn monitor_container_health(
    service_name: String,
    container_name: String,
    config: HealthCheckConfig,
    runtime: Arc<dyn ContainerRuntime>,
) {
    let health_store = CONTAINER_HEALTH
        .get()
        .expect("Health store not initialized");
    let health_cache = HEALTH_CACHE.get_or_init(DashMap::new);
    let mut consecutive_failures = 0;
    let mut backoff_start = None;

    // Initial startup period
    for i in 0..config.startup_failure_threshold {
        // Wait before each check (but skip wait on first iteration)
        if i > 0 {
            tokio::time::sleep(config.startup_timeout).await;
        }

        match runtime.inspect_container(&container_name).await {
            Ok(_) => {
                if let Some(mut status) = health_store.get_mut(&container_name) {
                    status.transition_to(HealthState::Starting, None);
                    // Update cache for Starting state
                    let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
                    health_cache.insert(
                        container_name.clone(),
                        (status.clone(), AtomicU64::new(generation)),
                    );
                }
                break;
            }
            Err(e) => {
                if let Some(mut status) = health_store.get_mut(&container_name) {
                    status.record_failure();
                    status
                        .transition_to(HealthState::Failed, Some(format!("Startup failed: {}", e)));
                    // Update cache for Failed state
                    let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
                    health_cache.insert(
                        container_name.clone(),
                        (status.clone(), AtomicU64::new(generation)),
                    );
                }
                return;
            }
        }
    }

    loop {
        let mut is_healthy = true;
        let container_stats = runtime.inspect_container(&container_name).await;
        let mut current_status = match health_store.get_mut(&container_name) {
            Some(status) => status,
            None => return, // Container removed, stop monitoring
        };

        // First check if container inspection succeeded
        match &container_stats {
            Ok(stats) => {
                // TCP health check if configured
                if let Some(tcp_check) = &config.tcp_check {
                    is_healthy =
                        check_tcp_health(&stats.ip_address, tcp_check.port, tcp_check.timeout)
                            .await;
                }

                if is_healthy {
                    consecutive_failures = 0;
                    backoff_start = None;
                    if !matches!(current_status.state, HealthState::Healthy) {
                        current_status.transition_to(HealthState::Healthy, None);
                        // Update cache for Healthy state
                        let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
                        health_cache.insert(
                            container_name.clone(),
                            (current_status.clone(), AtomicU64::new(generation)),
                        );
                    }
                } else {
                    consecutive_failures += 1;
                    current_status.record_failure();

                    if consecutive_failures >= config.liveness_failure_threshold {
                        if let Some(start) = backoff_start {
                            if SystemTime::now()
                                .duration_since(start)
                                .unwrap_or(Duration::from_secs(0))
                                < Duration::from_secs(300)
                            {
                                current_status.transition_to(
                                    HealthState::CrashLoopBackOff,
                                    Some("Container entering crash loop".to_string()),
                                );
                            }
                        } else {
                            backoff_start = Some(SystemTime::now());
                            current_status.transition_to(
                                HealthState::Unhealthy,
                                Some("Health check failed".to_string()),
                            );
                        }
                        // Update cache after any state change
                        let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
                        health_cache.insert(
                            container_name.clone(),
                            (current_status.clone(), AtomicU64::new(generation)),
                        );
                    }
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                current_status.record_failure();

                if consecutive_failures >= config.liveness_failure_threshold {
                    current_status.transition_to(
                        HealthState::Failed,
                        Some(format!("Container inspection failed: {}", e)),
                    );
                    // Update cache for Failed state
                    let generation = CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
                    health_cache.insert(
                        container_name.clone(),
                        (current_status.clone(), AtomicU64::new(generation)),
                    );
                    return;
                }
            }
        }

        tokio::time::sleep(config.liveness_period).await;
    }
}

pub fn get_container_health(container_name: &str) -> Option<ContainerHealthState> {
    HEALTH_CACHE
        .get()
        .and_then(|cache| match cache.try_get(container_name) {
            dashmap::try_result::TryResult::Present(entry) => Some(entry.value().0.clone()),
            _ => None,
        })
}
