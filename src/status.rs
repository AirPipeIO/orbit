// status.rs
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use crate::{
    config::get_config_by_service,
    container::{self, InstanceMetadata, INSTANCE_STORE},
    proxy::SERVER_BACKENDS,
};
use anyhow::Result;
use axum::Json;
use dashmap::DashMap;
use serde::Serialize;
use tokio::time::timeout;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct ContainerInfo {
    uuid: Uuid,
    exposed_port: u16,
    status: String,
    cpu_percentage: Option<f64>,
    cpu_percentage_relative: Option<f64>,
    memory_usage: Option<u64>,
    memory_limit: Option<u64>,
}

// Global cache for instance store data
pub static INSTANCE_STORE_CACHE: OnceLock<Arc<DashMap<String, HashMap<Uuid, InstanceMetadata>>>> =
    OnceLock::new();

pub fn update_instance_store_cache() -> Result<()> {
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised");

    // Create new DashMap
    let new_cache = Arc::new(DashMap::new());

    // Fill new cache
    for entry in instance_store.iter() {
        new_cache.insert(entry.key().clone(), entry.value().clone());
    }

    // Replace old cache with new one atomically
    INSTANCE_STORE_CACHE.get_or_init(|| new_cache);

    Ok(())
}

pub fn initialize_background_cache_update() {
    let log = slog_scope::logger();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            if let Err(e) = update_cache_with_timeout().await {
                slog::warn!(log, "Cache update failed"; "error" => e.to_string());
            }
        }
    });
}

// Replace the existing update_instance_store_cache with this version
async fn update_cache_with_timeout() -> Result<()> {
    // Add timeout to prevent blocking
    timeout(Duration::from_secs(5), async {
        let instance_store = INSTANCE_STORE
            .get()
            .expect("Instance store not initialised");
        let instance_cache = INSTANCE_STORE_CACHE.get_or_init(|| Arc::new(DashMap::new()));

        // Clear existing instance cache
        instance_cache.clear();

        // Update instance cache in batches
        for entry in instance_store.iter() {
            instance_cache.insert(entry.key().clone(), entry.value().clone());
            tokio::task::yield_now().await; // Allow other tasks to run
        }
    })
    .await?;

    Ok(())
}

#[derive(Serialize)]
pub struct ServiceStatus {
    service_name: String,
    service_ports: Vec<u16>,
    pods: Vec<PodStatus>,
}

#[derive(Serialize)]
pub struct PortStatus {
    pub port: u16,
    pub node_port: Option<u16>,
    pub healthy: bool,
}

#[derive(Serialize)]
pub struct PodStatus {
    uuid: Uuid,
    containers: Vec<ContainerStatus>,
}

#[derive(Serialize)]
pub struct ContainerStatus {
    name: String,
    ip_address: String,
    ports: Vec<PortStatus>,
    status: String,
    cpu_percentage: Option<f64>,
    cpu_percentage_relative: Option<f64>,
    memory_usage: Option<u64>,
    memory_limit: Option<u64>,
}

pub async fn get_status() -> Json<Vec<ServiceStatus>> {
    let instance_cache = INSTANCE_STORE_CACHE
        .get()
        .expect("Instance cache not initialized");
    let server_backends = SERVER_BACKENDS
        .get()
        .expect("Server backends not initialized");
    let service_stats = container::SERVICE_STATS
        .get()
        .expect("Service stats not initialized");

    let mut services = Vec::new();

    for entry in instance_cache.iter() {
        let service_name = entry.key();
        let instances = entry.value();

        let service_config = get_config_by_service(service_name);

        if let Some(config) = service_config {
            let service_stat = service_stats.get(service_name);

            // Collect all service ports
            let service_ports = config
                .spec
                .containers
                .iter()
                .flat_map(|c| c.ports.iter().flatten())
                .map(|p| p.port)
                .collect::<Vec<_>>();

            let pods: Vec<PodStatus> = instances
                .iter()
                .map(|(uuid, metadata)| {
                    let containers = metadata
                        .containers
                        .iter()
                        .map(|container| {
                            let container_stats = service_stat
                                .as_ref()
                                .and_then(|s| s.get_container_stats(&container.name));

                            let ports = container
                                .ports
                                .iter()
                                .map(|port_info| {
                                    let container_addr = format!("127.0.0.1:{}", port_info.port);
                                    let healthy = server_backends.iter().any(|backend_entry| {
                                        let proxy_key = format!(
                                            "{}_{}",
                                            service_name,
                                            port_info.node_port.unwrap_or(0)
                                        ); // !!! check
                                        backend_entry.key() == &proxy_key
                                            && backend_entry.value().iter().any(|backend| {
                                                backend.addr.to_string() == container_addr
                                            })
                                    });

                                    PortStatus {
                                        port: port_info.port,
                                        node_port: port_info.node_port,
                                        healthy,
                                    }
                                })
                                .collect();

                            ContainerStatus {
                                name: container.name.clone(),
                                ip_address: container.ip_address.clone(),
                                ports,
                                status: if container.ports.iter().any(|port_info| {
                                    let addr = format!("127.0.0.1:{}", port_info.port);
                                    let proxy_key = format!(
                                        "{}_{}",
                                        service_name,
                                        port_info.node_port.unwrap_or(0)
                                    ); // !!! check
                                    server_backends
                                        .get(&proxy_key)
                                        .map(|backends| {
                                            backends
                                                .iter()
                                                .any(|backend| backend.addr.to_string() == addr)
                                        })
                                        .unwrap_or(false)
                                }) {
                                    "running".to_string()
                                } else {
                                    "unknown".to_string()
                                },
                                cpu_percentage: container_stats.as_ref().map(|s| s.cpu_percentage),
                                cpu_percentage_relative: container_stats
                                    .as_ref()
                                    .map(|s| s.cpu_percentage_relative),
                                memory_usage: container_stats.as_ref().map(|s| s.memory_usage),
                                memory_limit: container_stats.as_ref().map(|s| s.memory_limit),
                            }
                        })
                        .collect();

                    PodStatus {
                        uuid: *uuid,
                        containers,
                    }
                })
                .collect();

            services.push(ServiceStatus {
                service_name: service_name.clone(),
                service_ports,
                pods,
            });
        }
    }

    Json(services)
}
