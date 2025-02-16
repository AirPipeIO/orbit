// src/api/status.rs

use crate::{
    config::get_config_by_service,
    container::{
        self,
        health::{self, ContainerHealthState},
        INSTANCE_STORE,
    },
    proxy::SERVER_BACKENDS,
};
use axum::Json;
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

pub async fn get_status() -> Json<Vec<ServiceStatus>> {
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialized");
    let server_backends = SERVER_BACKENDS
        .get()
        .expect("Server backends not initialized");
    let service_stats = container::SERVICE_STATS
        .get()
        .expect("Service stats not initialized");

    let mut services = Vec::new();

    // Get read lock for instance store
    let store_map = instance_store.read().await;

    for (service_name, instances) in store_map.iter() {
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

            let pods = futures::future::join_all(instances.iter().map(|(uuid, metadata)| async {
                let containers =
                    futures::future::join_all(metadata.containers.iter().map(|container| async {
                        let container_stats = service_stat
                            .as_ref()
                            .and_then(|s| s.get_container_stats(&container.name));

                        let mut urls = Vec::new();
                        for port_info in &container.ports {
                            let mut container_url = ContainerUrl {
                                url: format!("http://{}:{}", container.ip_address, port_info.port),
                                port: port_info.port,
                                target_url: None,
                                target_port: None,
                            };

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
                                        let proxy_key = format!("{}__{}", service_name, node_port);
                                        server_backends.iter().any(|backend_entry| {
                                            backend_entry.key() == &proxy_key
                                                && backend_entry.value().iter().any(|backend| {
                                                    backend.addr.to_string() == container_addr
                                                })
                                        })
                                    }
                                    None => {
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

                        let health_status = health::get_container_health(&container.name).await;

                        ContainerStatus {
                            urls,
                            health_status,
                            name: container.name.clone(),
                            ip_address: container.ip_address.clone(),
                            ports,
                            status: {
                                let has_node_port =
                                    container.ports.iter().any(|p| p.node_port.is_some());
                                if has_node_port {
                                    if container.ports.iter().any(|port_info| {
                                        let addr =
                                            format!("{}:{}", container.ip_address, port_info.port);
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
                                    if container.ports.is_empty() || container.ip_address.is_empty()
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
                    }))
                    .await;

                PodStatus {
                    uuid: *uuid,
                    containers,
                }
            }))
            .await;

            services.push(ServiceStatus {
                service_name: service_name.clone(),
                service_ports,
                service_urls,
                pods,
            });
        }
    }

    // Drop read lock explicitly here (though it would happen automatically)
    drop(store_map);

    Json(services)
}
