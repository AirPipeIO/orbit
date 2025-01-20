// metrics.rs
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use prometheus::{
    Counter, CounterVec, Encoder, GaugeVec, HistogramOpts, HistogramVec, IntGauge, IntGaugeVec,
    Opts, Registry,
};
use rustc_hash::FxHashMap;
use std::{error::Error, sync::OnceLock, time::Duration};

use crate::{container::INSTANCE_STORE, status::CONTAINER_STATS_CACHE};

use tokio::sync::mpsc;

// Global channel for metrics updates
pub static METRICS_SENDER: OnceLock<mpsc::Sender<MetricsUpdate>> = OnceLock::new();

// Enum to represent different types of metrics updates
#[derive(Debug)]
pub enum MetricsUpdate {
    ServiceStats(String, ServiceStats),
    TotalServices(usize),
    TotalInstances(usize),
    ConfigReload,
    Request(String, u16),              // service_name, status_code
    RequestDuration(String, u16, f64), // service_name, status_code, duration
}

#[derive(Debug, Default, Clone)]
pub struct ServiceStats {
    pub instance_count: usize,
    pub cpu_usage: FxHashMap<String, f64>,
    pub memory_usage: FxHashMap<String, u64>,
    pub active_connections: i64,
}

// Global registry
pub static REGISTRY: OnceLock<Registry> = OnceLock::new();

// Global metrics
pub static TOTAL_REQUESTS: OnceLock<Counter> = OnceLock::new();
pub static TOTAL_SERVICES: OnceLock<IntGauge> = OnceLock::new();
pub static TOTAL_INSTANCES: OnceLock<IntGauge> = OnceLock::new();
pub static CONFIG_RELOADS: OnceLock<Counter> = OnceLock::new();

// Per-service metrics
pub static SERVICE_INSTANCES: OnceLock<IntGaugeVec> = OnceLock::new();
pub static SERVICE_CPU_USAGE: OnceLock<GaugeVec> = OnceLock::new();
pub static SERVICE_MEMORY_USAGE: OnceLock<GaugeVec> = OnceLock::new();
pub static SERVICE_REQUEST_DURATION: OnceLock<HistogramVec> = OnceLock::new();
pub static SERVICE_ACTIVE_CONNECTIONS: OnceLock<IntGaugeVec> = OnceLock::new();
pub static SERVICE_REQUEST_TOTAL: OnceLock<CounterVec> = OnceLock::new();

pub fn initialize_metrics() -> Result<(), Box<dyn Error>> {
    let registry = Registry::new();

    // Initialize global metrics
    let total_requests =
        Counter::new("orbit_requests_total", "Total number of requests processed")?;
    registry.register(Box::new(total_requests.clone()))?;
    TOTAL_REQUESTS.set(total_requests).unwrap();

    let total_services = IntGauge::new(
        "orbit_services_total",
        "Total number of services being managed",
    )?;
    registry.register(Box::new(total_services.clone()))?;
    TOTAL_SERVICES.set(total_services).unwrap();

    let total_instances = IntGauge::new(
        "orbit_instances_total",
        "Total number of container instances",
    )?;
    registry.register(Box::new(total_instances.clone()))?;
    TOTAL_INSTANCES.set(total_instances).unwrap();

    let config_reloads = Counter::new(
        "orbit_config_reloads_total",
        "Total number of configuration reloads",
    )?;
    registry.register(Box::new(config_reloads.clone()))?;
    CONFIG_RELOADS.set(config_reloads).unwrap();

    // Initialize per-service metrics
    let service_instances = IntGaugeVec::new(
        Opts::new("orbit_service_instances", "Number of instances per service"),
        &["service"],
    )?;
    registry.register(Box::new(service_instances.clone()))?;
    SERVICE_INSTANCES.set(service_instances).unwrap();

    let service_cpu_usage = GaugeVec::new(
        Opts::new(
            "orbit_service_cpu_usage_percent",
            "CPU usage percentage per service instance",
        ),
        &["service", "instance"],
    )?;
    registry.register(Box::new(service_cpu_usage.clone()))?;
    SERVICE_CPU_USAGE.set(service_cpu_usage).unwrap();

    let service_memory_usage = GaugeVec::new(
        Opts::new(
            "orbit_service_memory_bytes",
            "Memory usage in bytes per service instance",
        ),
        &["service", "instance"],
    )?;
    registry.register(Box::new(service_memory_usage.clone()))?;
    SERVICE_MEMORY_USAGE.set(service_memory_usage).unwrap();

    let service_request_duration = HistogramVec::new(
        HistogramOpts::new(
            "orbit_service_request_duration_seconds",
            "Request duration in seconds per service",
        ),
        &["service", "status"],
    )?;
    registry.register(Box::new(service_request_duration.clone()))?;
    SERVICE_REQUEST_DURATION
        .set(service_request_duration)
        .unwrap();

    let service_active_connections = IntGaugeVec::new(
        Opts::new(
            "orbit_service_active_connections",
            "Number of active connections per service",
        ),
        &["service"],
    )?;
    registry.register(Box::new(service_active_connections.clone()))?;
    SERVICE_ACTIVE_CONNECTIONS
        .set(service_active_connections)
        .unwrap();

    let service_request_total = CounterVec::new(
        Opts::new("orbit_service_requests_total", "Total requests per service"),
        &["service", "status"],
    )?;
    registry.register(Box::new(service_request_total.clone()))?;
    SERVICE_REQUEST_TOTAL.set(service_request_total).unwrap();

    // Set the global registry
    REGISTRY.set(registry).unwrap();

    // Create channel for metrics updates
    let (tx, mut rx) = mpsc::channel(1000); // Buffer size of 1000
    METRICS_SENDER.set(tx).unwrap();

    // Spawn metrics processing task
    tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            process_metrics_update(update);
        }
    });

    Ok(())
}

// Update metrics for a service
pub fn update_service_metrics(service_name: &str, stats: &ServiceStats) {
    if let Some(instances) = SERVICE_INSTANCES.get() {
        instances
            .with_label_values(&[service_name])
            .set(stats.instance_count as i64);
    }

    if let Some(cpu_usage) = SERVICE_CPU_USAGE.get() {
        for (instance_id, cpu) in &stats.cpu_usage {
            cpu_usage
                .with_label_values(&[service_name, instance_id])
                .set(*cpu);
        }
    }

    if let Some(memory_usage) = SERVICE_MEMORY_USAGE.get() {
        for (instance_id, mem) in &stats.memory_usage {
            memory_usage
                .with_label_values(&[service_name, instance_id])
                .set(*mem as f64);
        }
    }
}

// Handler for metrics endpoint with timeout
pub async fn metrics_handler() -> axum::response::Response {
    use tokio::time::timeout;

    match timeout(Duration::from_secs(5), collect_metrics()).await {
        Ok(result) => result,
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "Metrics collection timed out".to_string(),
        )
            .into_response(),
    }
}

// Collect metrics without holding locks
async fn collect_metrics() -> axum::response::Response {
    if let Some(registry) = REGISTRY.get() {
        let encoder = prometheus::TextEncoder::new();
        let mut buffer = Vec::new();

        match encoder.encode(&registry.gather(), &mut buffer) {
            Ok(_) => match String::from_utf8(buffer) {
                Ok(metrics_text) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                    metrics_text,
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "text/plain")],
                    format!("Failed to convert metrics to UTF-8: {}", e),
                )
                    .into_response(),
            },
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("Failed to encode metrics: {}", e),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "Metrics registry not initialized".to_string(),
        )
            .into_response()
    }
}

// Process metrics updates without holding locks
fn process_metrics_update(update: MetricsUpdate) {
    match update {
        MetricsUpdate::ServiceStats(service_name, stats) => {
            if let Some(instances) = SERVICE_INSTANCES.get() {
                instances
                    .with_label_values(&[&service_name])
                    .set(stats.instance_count as i64);
            }

            if let Some(cpu_usage) = SERVICE_CPU_USAGE.get() {
                for (instance_id, cpu) in &stats.cpu_usage {
                    cpu_usage
                        .with_label_values(&[&service_name, instance_id])
                        .set(*cpu);
                }
            }

            if let Some(memory_usage) = SERVICE_MEMORY_USAGE.get() {
                for (instance_id, mem) in &stats.memory_usage {
                    memory_usage
                        .with_label_values(&[&service_name, instance_id])
                        .set(*mem as f64);
                }
            }
        }
        MetricsUpdate::TotalServices(count) => {
            if let Some(total_services) = TOTAL_SERVICES.get() {
                total_services.set(count as i64);
            }
        }
        MetricsUpdate::TotalInstances(count) => {
            if let Some(total_instances) = TOTAL_INSTANCES.get() {
                total_instances.set(count as i64);
            }
        }
        MetricsUpdate::ConfigReload => {
            if let Some(config_reloads) = CONFIG_RELOADS.get() {
                config_reloads.inc();
            }
        }
        MetricsUpdate::Request(service_name, status_code) => {
            if let Some(requests) = SERVICE_REQUEST_TOTAL.get() {
                requests
                    .with_label_values(&[&service_name, &status_code.to_string()])
                    .inc();
            }
        }
        MetricsUpdate::RequestDuration(service_name, status_code, duration) => {
            if let Some(durations) = SERVICE_REQUEST_DURATION.get() {
                durations
                    .with_label_values(&[&service_name, &status_code.to_string()])
                    .observe(duration);
            }
        }
    }
}

// Helper function to send metrics updates
pub async fn send_metrics_update(update: MetricsUpdate) {
    if let Some(sender) = METRICS_SENDER.get() {
        let _ = sender.send(update).await;
    }
}
