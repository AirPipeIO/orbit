// rolling_updates.rs

use anyhow::{anyhow, Result};
use dashmap::{DashMap, DashSet};
use pingora_load_balancing::Backend;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::interval;
use uuid::Uuid;

use crate::{
    config::{
        get_config_by_service, parse_container_name, ScaleMessage, ServiceConfig, CONFIG_UPDATES,
    },
    container::{
        get_next_pod_number, ContainerMetadata, ContainerPortMetadata, ContainerRuntime,
        InstanceMetadata, INSTANCE_STORE, RUNTIME,
    },
    proxy::SERVER_BACKENDS,
    scale::scale_down,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateState {
    pub in_progress: bool,
    pub current_pod: Option<Uuid>,
    pub updated_pods: Vec<Uuid>,
    pub start_time: Option<SystemTime>,
}

pub async fn start_image_check_task(service_name: String, config: ServiceConfig) -> Result<()> {
    let runtime = RUNTIME.get().unwrap();
    let mut last_image_hashes = HashMap::new();
    let check_interval = config
        .image_check_interval
        .unwrap_or(Duration::from_secs(300));
    let mut interval = interval(check_interval);

    loop {
        interval.tick().await;

        let current_config = match get_config_by_service(&service_name) {
            Some(cfg) => cfg,
            None => break,
        };

        // Get current image hashes once
        let mut current_hashes = HashMap::new();
        for container in &current_config.spec.containers {
            if let Ok(hash) = runtime.get_image_digest(&container.image).await {
                current_hashes.insert(container.name.clone(), hash);
            }
        }

        // Only trigger update if hashes changed
        if !last_image_hashes.is_empty() && current_hashes != last_image_hashes {
            slog::info!(slog_scope::logger(), "Image updates detected";
                "service" => &service_name
            );

            if let Some(sender) = CONFIG_UPDATES.get() {
                sender
                    .send((service_name.clone(), ScaleMessage::RollingUpdate))
                    .await?;
            }

            perform_rolling_update(
                &service_name,
                &current_config,
                runtime.clone(),
                &current_hashes,
            )
            .await?;

            if let Some(sender) = CONFIG_UPDATES.get() {
                sender
                    .send((service_name.clone(), ScaleMessage::RollingUpdateComplete))
                    .await?;
            }
        }

        last_image_hashes = current_hashes;
    }

    Ok(())
}

async fn perform_rolling_update(
    service_name: &str,
    config: &ServiceConfig,
    runtime: Arc<dyn ContainerRuntime>,
    new_image_hashes: &HashMap<String, String>,
) -> Result<()> {
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialized");
    let server_backends = SERVER_BACKENDS
        .get()
        .expect("Server backends not initialized");
    let log = slog_scope::logger();

    // Get pods without holding lock
    let pods: Vec<_> = {
        let instances = instance_store
            .get(service_name)
            .ok_or_else(|| anyhow!("Service not found"))?;
        instances
            .value()
            .iter()
            .map(|(uuid, metadata)| (*uuid, metadata.clone()))
            .collect()
    };

    for (pod_uuid, old_metadata) in pods {
        // Create new pod
        let pod_number = get_next_pod_number(service_name).await;
        let new_containers = runtime
            .start_containers(service_name, pod_number, &config.spec.containers, config)
            .await?;

        if !new_containers.is_empty() {
            let new_uuid = parse_container_name(&new_containers[0].0)?.uuid;

            // Add new pod metadata
            {
                let mut instances = instance_store
                    .get_mut(service_name)
                    .ok_or_else(|| anyhow!("Service not found"))?;
                instances.insert(
                    new_uuid,
                    InstanceMetadata {
                        uuid: new_uuid,
                        created_at: SystemTime::now(),
                        network: format!("{}_{}", service_name, new_uuid),
                        image_hash: new_image_hashes.clone(),
                        containers: new_containers
                            .iter()
                            .map(|(name, ip, ports)| ContainerMetadata {
                                name: name.clone(),
                                network: format!("{}_{}", service_name, new_uuid),
                                ip_address: ip.clone(),
                                ports: ports.clone(),
                                status: "running".to_string(),
                            })
                            .collect(),
                    },
                );
            }

            // Add new containers to LB
            add_to_load_balancer(service_name, &new_containers, server_backends)?;
            tokio::time::sleep(Duration::from_secs(5)).await;

            // Remove old pod metadata
            {
                let mut instances = instance_store
                    .get_mut(service_name)
                    .ok_or_else(|| anyhow!("Service not found"))?;
                instances.remove(&pod_uuid);
            }

            // Remove old containers from LB
            remove_from_load_balancer(service_name, &old_metadata.containers, server_backends)?;
            tokio::time::sleep(Duration::from_secs(10)).await;

            // Cleanup old pod
            cleanup_pod(&old_metadata, service_name, runtime.clone()).await?;
        }
    }

    Ok(())
}

fn add_to_load_balancer(
    service_name: &str,
    containers: &[(String, String, Vec<ContainerPortMetadata>)],
    server_backends: &DashMap<String, Arc<DashSet<Backend>>>,
) -> Result<()> {
    for (_, ip, ports) in containers {
        for port_info in ports {
            if let Some(node_port) = port_info.node_port {
                let proxy_key = format!("{}_{}", service_name, node_port);
                if let Some(backends) = server_backends.get(&proxy_key) {
                    let addr = format!("{}:{}", ip, port_info.port);
                    if let Ok(backend) = Backend::new(&addr) {
                        backends.insert(backend);
                    }
                }
            }
        }
    }
    Ok(())
}

fn remove_from_load_balancer(
    service_name: &str,
    containers: &[ContainerMetadata],
    server_backends: &DashMap<String, Arc<DashSet<Backend>>>,
) -> Result<()> {
    for container in containers {
        for port_info in &container.ports {
            if let Some(node_port) = port_info.node_port {
                let proxy_key = format!("{}_{}", service_name, node_port);
                if let Some(backends) = server_backends.get(&proxy_key) {
                    let addr = format!("{}:{}", container.ip_address, port_info.port);
                    if let Ok(backend) = Backend::new(&addr) {
                        backends.remove(&backend);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn cleanup_pod(
    metadata: &InstanceMetadata,
    service_name: &str,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<()> {
    let mut service_uuid = String::new();

    for container in &metadata.containers {
        service_uuid = parse_container_name(&container.name)?.uuid.to_string();

        if let Err(e) = runtime.stop_container(&container.name).await {
            slog::error!(slog_scope::logger(), "Failed to stop container";
                "service" => service_name,
                "container" => &container.name,
                "error" => e.to_string()
            );
        }
    }

    let network_name = format!("{}__{}", service_name, service_uuid);

    if let Err(e) = runtime.remove_pod_network(&network_name).await {
        slog::error!(slog_scope::logger(), "Failed to remove pod network";
            "service" => service_name,
            "network" => &metadata.network,
            "error" => e.to_string()
        );
    }
    Ok(())
}
