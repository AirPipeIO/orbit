// metrics.rs
use axum::{
    http::{header, StatusCode},
    response::IntoResponse,
};
use prometheus::{
    Counter, CounterVec, GaugeVec, HistogramOpts, HistogramVec, IntGauge, IntGaugeVec, Opts,
    Registry,
};
use rustc_hash::FxHashMap;
use std::{error::Error, sync::OnceLock};

use crate::{container::INSTANCE_STORE, status::CONTAINER_STATS_CACHE};

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

pub fn setup_metrics() -> Result<(), Box<dyn Error>> {
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

    Ok(())
}

// Helper struct for updating service metrics
#[derive(Default)]
pub struct ServiceStats {
    pub instance_count: usize,
    pub cpu_usage: FxHashMap<String, f64>,
    pub memory_usage: FxHashMap<String, u64>,
    pub active_connections: i64,
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

// Handler for the metrics endpoint
pub async fn metrics_handler() -> impl IntoResponse {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();

    let mut buffer = Vec::new();
    if let Some(registry) = REGISTRY.get() {
        if let Err(e) = encoder.encode(&registry.gather(), &mut buffer) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to encode metrics: {}", e),
            )
                .into_response();
        }
    }

    match String::from_utf8(buffer) {
        Ok(metrics_text) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            metrics_text,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to convert metrics to UTF-8: {}", e),
        )
            .into_response(),
    }
}

// Update function to collect metrics from container stats
pub fn collect_service_metrics() {
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialized");
    let stats_cache = CONTAINER_STATS_CACHE
        .get()
        .expect("Stats cache not initialized");

    if let Some(total_instances) = TOTAL_INSTANCES.get() {
        let count: i64 = instance_store
            .iter()
            .map(|entry| entry.value().len() as i64)
            .sum();
        total_instances.set(count);
    }

    if let Some(total_services) = TOTAL_SERVICES.get() {
        total_services.set(instance_store.len() as i64);
    }

    for entry in instance_store.iter() {
        let service_name = entry.key();
        let instances = entry.value();

        let mut stats = ServiceStats {
            instance_count: instances.len(),
            ..Default::default()
        };

        for (uuid, metadata) in instances {
            let container_name = format!("{}_{}", service_name, uuid);
            if let Some(container_stats) = stats_cache.get(&container_name) {
                stats
                    .cpu_usage
                    .insert(uuid.to_string(), container_stats.cpu_percentage);
                stats
                    .memory_usage
                    .insert(uuid.to_string(), container_stats.memory_usage);
            }
        }

        update_service_metrics(service_name, &stats);
    }
}
