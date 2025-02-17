// src/container/rolling_update.rs

use anyhow::{anyhow, Result};
use pingora_load_balancing::Backend;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::time::interval;
use uuid::Uuid;

use crate::{
    config::{
        get_config_by_service, parse_container_name, ScaleMessage, ServiceConfig, CONFIG_UPDATES,
    },
    container::{
        get_next_pod_number, ContainerMetadata, ContainerRuntime, InstanceMetadata, INSTANCE_STORE,
        RUNTIME,
    },
    proxy::SERVER_BACKENDS,
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

        let current_config = match get_config_by_service(&service_name).await {
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

    // Get pods with read lock
    let pods = {
        let store = instance_store.read().await;
        match store.get(service_name) {
            Some(instances) => instances
                .iter()
                .map(|(uuid, metadata)| (*uuid, metadata.clone()))
                .collect::<Vec<_>>(),
            None => return Err(anyhow!("Service not found")),
        }
    };

    let total_pods = pods.len();
    let update_config = config.rolling_update_config.clone().unwrap_or_default();
    let max_surge = update_config.max_surge as usize;
    let timeout = update_config.timeout;

    // Calculate how many new pods we can create at once based on max_surge
    let allowed_new_pods = (total_pods + max_surge).saturating_sub(total_pods);
    let new_pod_count = total_pods.min(allowed_new_pods);

    // Create all new pods in parallel
    let mut new_pod_futures = Vec::new();
    let mut pod_numbers = Vec::new();
    for _ in 0..new_pod_count {
        pod_numbers.push(get_next_pod_number(service_name).await);
    }

    for pod_number in pod_numbers {
        let runtime = runtime.clone();
        let config = config.clone();
        let service_name = service_name.to_string();

        new_pod_futures.push(tokio::spawn(async move {
            runtime
                .start_containers(&service_name, pod_number, &config.spec.containers, &config)
                .await
        }));
    }

    // Collect results and update instance store
    let mut new_pods = Vec::new();
    for future in new_pod_futures {
        match future.await? {
            Ok(new_containers) => {
                if !new_containers.is_empty() {
                    let new_uuid = parse_container_name(&new_containers[0].0)?.uuid;
                    let network_name = format!("{}__{}", service_name, new_uuid);

                    // Update instance store with write lock
                    {
                        let mut store = instance_store.write().await;
                        if let Some(instances) = store.get_mut(service_name) {
                            instances.insert(
                                new_uuid,
                                InstanceMetadata {
                                    uuid: new_uuid,
                                    created_at: SystemTime::now(),
                                    network: network_name.clone(),
                                    image_hash: new_image_hashes.clone(),
                                    containers: new_containers
                                        .iter()
                                        .map(|(name, ip, ports)| ContainerMetadata {
                                            name: name.clone(),
                                            network: network_name.clone(),
                                            ip_address: ip.clone(),
                                            ports: ports.clone(),
                                            status: "running".to_string(),
                                        })
                                        .collect(),
                                },
                            );
                        }
                    }
                    new_pods.push((new_uuid, new_containers));
                }
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Update load balancer for all new pods
    for (_, containers) in &new_pods {
        for (_, ip, ports) in containers {
            for port_info in ports {
                if let Some(node_port) = port_info.node_port {
                    let proxy_key = format!("{}__{}", service_name, node_port);

                    let backends = {
                        let backends_map = server_backends.read().await;
                        backends_map.get(&proxy_key).cloned()
                    };

                    if let Some(backends) = backends {
                        let addr = format!("{}:{}", ip, port_info.port);
                        if let Ok(backend) = Backend::new(&addr) {
                            let mut backend_set = backends.write().await;
                            backend_set.insert(backend);
                        }
                    }
                }
            }
        }
    }

    // Wait for new pods to be ready
    let start = Instant::now();
    while start.elapsed() < timeout {
        let mut all_ready = true;
        for (_, containers) in &new_pods {
            for (name, _, _) in containers {
                if runtime.inspect_container(name).await.is_err() {
                    all_ready = false;
                    break;
                }
            }
        }
        if all_ready {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Remove old pods one by one
    for (old_uuid, old_metadata) in pods {
        // Remove from load balancer
        for container in &old_metadata.containers {
            for port_info in &container.ports {
                if let Some(node_port) = port_info.node_port {
                    let proxy_key = format!("{}__{}", service_name, node_port);

                    let backends = {
                        let backends_map = server_backends.read().await;
                        backends_map.get(&proxy_key).cloned()
                    };

                    if let Some(backends) = backends {
                        let addr = format!("{}:{}", container.ip_address, port_info.port);
                        if let Ok(backend) = Backend::new(&addr) {
                            let mut backend_set = backends.write().await;
                            backend_set.remove(&backend);
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;

        // Remove from instance store with write lock
        {
            let mut store = instance_store.write().await;
            if let Some(instances) = store.get_mut(service_name) {
                instances.remove(&old_uuid);
            }
        }

        // Clean up containers and network
        let _ = cleanup_pod(&old_metadata, service_name, runtime.clone()).await;
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

    if let Err(e) = runtime
        .remove_pod_network(&network_name, service_name)
        .await
    {
        slog::error!(slog_scope::logger(), "Failed to remove pod network";
            "service" => service_name,
            "network" => &metadata.network,
            "error" => e.to_string()
        );
    }
    Ok(())
}
