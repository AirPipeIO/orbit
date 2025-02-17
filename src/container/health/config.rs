// src/container/health/config.rs
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    #[serde(with = "humantime_serde", default = "default_startup_timeout")]
    pub startup_timeout: Duration,
    #[serde(default = "default_startup_threshold")]
    pub startup_failure_threshold: u32,
    #[serde(with = "humantime_serde", default = "default_liveness_period")]
    pub liveness_period: Duration,
    #[serde(default = "default_liveness_threshold")]
    pub liveness_failure_threshold: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp_check: Option<TcpHealthCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpHealthCheck {
    pub port: u16,
    #[serde(with = "humantime_serde", default = "default_tcp_timeout")]
    pub timeout: Duration,
    #[serde(with = "humantime_serde", default = "default_tcp_period")]
    pub period: Duration,
    #[serde(default = "default_tcp_threshold")]
    pub failure_threshold: u32,
}

fn default_startup_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_startup_threshold() -> u32 {
    3
}
fn default_liveness_period() -> Duration {
    Duration::from_secs(10)
}
fn default_liveness_threshold() -> u32 {
    3
}
fn default_tcp_timeout() -> Duration {
    Duration::from_secs(5)
}
fn default_tcp_period() -> Duration {
    Duration::from_secs(10)
}
fn default_tcp_threshold() -> u32 {
    3
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            startup_timeout: default_startup_timeout(),
            startup_failure_threshold: default_startup_threshold(),
            liveness_period: default_liveness_period(),
            liveness_failure_threshold: default_liveness_threshold(),
            tcp_check: None,
        }
    }
}
