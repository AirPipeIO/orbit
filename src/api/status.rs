// src/api/status.rs
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use crate::{
    config::get_config_by_service,
    container::{
        self,
        health::{self, ContainerHealthState},
        InstanceMetadata, INSTANCE_STORE,
    },
    proxy::SERVER_BACKENDS,
};
use anyhow::Result;
use axum::Json;
use dashmap::DashMap;
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
pub struct ServiceUrl {
    pub url: String,
    pub node_port: u16,
}

#[derive(Serialize)]
pub struct ContainerUrl {
    pub url: String,
    pub port: u16,
    pub target_url: Option<String>,
    pub target_port: Option<u16>,
}

#[derive(Serialize)]
pub struct ServiceStatus {
    pub service_name: String,
    pub service_ports: Vec<u16>,
    pub service_urls: Vec<ServiceUrl>, // Add this field
    pub pods: Vec<PodStatus>,
}

#[derive(Serialize)]
pub struct PortStatus {
    pub port: u16,
    pub target_port: Option<u16>,
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
    pub name: String,
    pub ip_address: String,
    pub ports: Vec<PortStatus>,
    pub status: String,
    pub cpu_percentage: Option<f64>,
    pub cpu_percentage_relative: Option<f64>,
    pub memory_usage: Option<u64>,
    pub memory_limit: Option<u64>,
    pub urls: Vec<ContainerUrl>,
    pub health_status: Option<ContainerHealthState>,
}

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
        .ok_or_else(|| anyhow::anyhow!("Instance store not initialized"))?;

    // Create new data structure without holding locks
    let new_data: HashMap<String, HashMap<Uuid, InstanceMetadata>> = instance_store
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();

    // Create new DashMap
    let new_cache = Arc::new(DashMap::from_iter(new_data.clone()));

    // Attempt to set the new cache
    match INSTANCE_STORE_CACHE.set(new_cache) {
        Ok(_) => Ok(()),
        Err(_) => {
            // If we couldn't set it (already set), update the existing one
            if let Some(existing_cache) = INSTANCE_STORE_CACHE.get() {
                existing_cache.clear();
                for (key, value) in new_data {
                    existing_cache.insert(key, value);
                }
            }
            Ok(())
        }
    }
}

pub fn initialize_background_cache_update() {
    let log = slog_scope::logger();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        let mut consecutive_failures = 0;

        loop {
            interval.tick().await;

            match tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(update_instance_store_cache),
            )
            .await
            {
                Ok(Ok(Ok(_))) => {
                    consecutive_failures = 0;
                }
                error => {
                    consecutive_failures += 1;
                    slog::error!(log, "Cache update failed";
                        "error" => format!("{:?}", error),
                        "consecutive_failures" => consecutive_failures.to_string()
                    );

                    // If we've failed multiple times, increase the interval temporarily
                    if consecutive_failures > 3 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    });
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

            // Collect all service ports and URLs
            let mut service_ports = Vec::new();
            let mut service_urls = Vec::new();

            for container in &config.spec.containers {
                if let Some(ports) = &container.ports {
                    for port_config in ports {
                        service_ports.push(port_config.port);

                        // Add service URLs for node ports
                        if let Some(node_port) = port_config.node_port {
                            service_urls.push(ServiceUrl {
                                url: format!("http://localhost:{}", node_port),
                                node_port,
                            });
                        }
                    }
                }
            }

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

                            let mut urls = Vec::new();
                            for port_info in &container.ports {
                                let mut container_url = ContainerUrl {
                                    url: format!(
                                        "http://{}:{}",
                                        container.ip_address, port_info.port
                                    ),
                                    port: port_info.port,
                                    target_url: None,
                                    target_port: None,
                                };

                                // Add target URL if target_port is defined
                                if let Some(target_port) = port_info.target_port {
                                    container_url.target_url = Some(format!(
                                        "http://{}:{}",
                                        container.ip_address, target_port
                                    ));
                                    container_url.target_port = Some(target_port);
                                }

                                urls.push(container_url);
                            }

                            let ports = container
                                .ports
                                .iter()
                                .map(|port_info| {
                                    let container_addr =
                                        format!("{}:{}", container.ip_address, port_info.port);
                                    let healthy = match port_info.node_port {
                                        Some(node_port) => {
                                            // Check load balancer registration for node_port
                                            let proxy_key =
                                                format!("{}__{}", service_name, node_port);
                                            server_backends.iter().any(|backend_entry| {
                                                backend_entry.key() == &proxy_key
                                                    && backend_entry.value().iter().any(|backend| {
                                                        backend.addr.to_string() == container_addr
                                                    })
                                            })
                                        }
                                        None => {
                                            // For target_port, check if IP and port are valid
                                            !container_addr.is_empty()
                                                && container.ip_address != "0.0.0.0"
                                                && port_info.target_port.is_some()
                                        }
                                    };

                                    PortStatus {
                                        port: port_info.port,
                                        target_port: port_info.target_port,
                                        node_port: port_info.node_port,
                                        healthy,
                                    }
                                })
                                .collect();
                            ContainerStatus {
                                urls,
                                health_status: health::get_container_health(&container.name),
                                name: container.name.clone(),
                                ip_address: container.ip_address.clone(),
                                ports,
                                status: {
                                    let has_node_port =
                                        container.ports.iter().any(|p| p.node_port.is_some());
                                    if has_node_port {
                                        // For containers with node_ports, check if registered with load balancer
                                        if container.ports.iter().any(|port_info| {
                                            let addr = format!(
                                                "{}:{}",
                                                container.ip_address, port_info.port
                                            );
                                            if let Some(node_port) = port_info.node_port {
                                                let proxy_key =
                                                    format!("{}__{}", service_name, node_port);
                                                server_backends
                                                    .get(&proxy_key)
                                                    .map(|backends| {
                                                        backends.iter().any(|backend| {
                                                            backend.addr.to_string() == addr
                                                        })
                                                    })
                                                    .unwrap_or(false)
                                            } else {
                                                false
                                            }
                                        }) {
                                            "running".to_string()
                                        } else {
                                            "stopped".to_string()
                                        }
                                    } else {
                                        // For containers without node_ports, consider running if they have valid ports
                                        if container.ports.is_empty()
                                            || container.ip_address.is_empty()
                                        {
                                            "stopped".to_string()
                                        } else {
                                            "running".to_string()
                                        }
                                    }
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
                service_urls,
                pods,
            });
        }
    }

    Json(services)
}
