// src/container/health/mod.rs
use crate::container::ContainerRuntime;
use anyhow::Result;
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::RwLock;

pub use self::config::HealthCheckConfig;
use super::RUNTIME;
mod config;

pub static CONTAINER_HEALTH: OnceLock<Arc<RwLock<FxHashMap<String, ContainerHealthState>>>> =
    OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub enum HealthState {
    Starting,
    Healthy,
    Unhealthy,
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

// Update initialize_health_monitoring
pub async fn initialize_health_monitoring(
    service_name: &str,
    container_name: &str,
    config: Option<HealthCheckConfig>,
) -> Result<()> {
    let health_store = CONTAINER_HEALTH.get_or_init(|| Arc::new(RwLock::new(FxHashMap::default())));
    let config = config.unwrap_or_default();

    // Initialize health state
    {
        let mut health_map = health_store.write().await;
        health_map.insert(container_name.to_string(), ContainerHealthState::default());
    }

    slog::info!(slog_scope::logger(), "Health monitoring initialized";
        "service" => service_name,
        "container" => container_name,
    );

    // Spawn monitoring task
    tokio::spawn(monitor_container_health(
        service_name.to_string(),
        container_name.to_string(),
        config,
        RUNTIME.get().expect("Runtime not initialized").clone(),
    ));

    Ok(())
}

async fn check_tcp_health(addr: &str, port: u16, timeout: Duration) -> bool {
    match tokio::time::timeout(timeout, TcpStream::connect(format!("{}:{}", addr, port))).await {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

// Update monitor_container_health function
async fn monitor_container_health(
    _service_name: String,
    container_name: String,
    config: HealthCheckConfig,
    runtime: Arc<dyn ContainerRuntime>,
) {
    let health_store = CONTAINER_HEALTH
        .get()
        .expect("Health store not initialized");
    let mut consecutive_failures = 0;

    // Initial startup period
    for i in 0..config.startup_failure_threshold {
        // Wait before each check (but skip wait on first iteration)
        if i > 0 {
            tokio::time::sleep(config.startup_timeout).await;
        }

        match runtime.inspect_container(&container_name).await {
            Ok(_) => {
                {
                    let mut health_map = health_store.write().await;
                    if let Some(status) = health_map.get_mut(&container_name) {
                        status.transition_to(HealthState::Starting, None);
                    }
                }
                break;
            }
            Err(e) => {
                let mut health_map = health_store.write().await;
                if let Some(status) = health_map.get_mut(&container_name) {
                    status.record_failure();
                    status
                        .transition_to(HealthState::Failed, Some(format!("Startup failed: {}", e)));
                }
                return;
            }
        }
    }

    loop {
        let mut is_healthy = true;
        let container_stats = runtime.inspect_container(&container_name).await;

        {
            let mut health_map = health_store.write().await;
            let current_status = match health_map.get_mut(&container_name) {
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
                        if !matches!(current_status.state, HealthState::Healthy) {
                            current_status.transition_to(HealthState::Healthy, None);
                        }
                    } else {
                        consecutive_failures += 1;
                        current_status.record_failure();

                        if consecutive_failures >= config.liveness_failure_threshold {
                            current_status.transition_to(
                                HealthState::Unhealthy,
                                Some("Health check failed".to_string()),
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
                        return;
                    }
                }
            }
        }

        tokio::time::sleep(config.liveness_period).await;
    }
}

// Example read operation
pub async fn get_container_health(container_name: &str) -> Option<ContainerHealthState> {
    if let Some(store) = CONTAINER_HEALTH.get() {
        let health_map = store.read().await;
        return health_map.get(container_name).cloned();
    }
    None
}
