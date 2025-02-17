// src/container/scaling/mod.rs
pub mod codel;
pub mod manager;
use anyhow::Result;
use codel::get_service_metrics;
use manager::{ScalingDecision, UnifiedScalingManager};
use pingora_load_balancing::Backend;
use rustc_hash::FxHashMap;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    config::{
        aggregate_pod_stats, get_config_by_service, parse_container_name, PodMetricsStrategy,
        ScaleMessage, ServiceConfig, CONFIG_UPDATES,
    },
    container::{
        get_next_pod_number,
        health::{self},
        ContainerMetadata, ContainerRuntime, InstanceMetadata, INSTANCE_STORE, RUNTIME,
    },
    proxy::{run_proxy_for_service, SERVER_BACKENDS},
};

use super::health::CONTAINER_HEALTH;

pub async fn auto_scale(service_name: String) {
    let log = slog_scope::logger();
    let runtime = RUNTIME.get().unwrap().clone();

    let (tx, mut rx) = mpsc::channel(100);
    CONFIG_UPDATES.get_or_init(|| tx);

    let mut scaling_paused = false;
    let service_name = Arc::new(service_name);

    // Initialize the scaling manager
    let mut scaling_manager = {
        let config = match get_config_by_service(&service_name).await {
            Some(cfg) => cfg,
            None => {
                slog::error!(log, "Service config not found, stopping auto_scale";
                    "service" => service_name.as_str());
                return;
            }
        };

        let codel_metrics = if let Some(codel_config) = &config.codel {
            Some(get_service_metrics(&service_name, codel_config).await)
        } else {
            None
        };

        UnifiedScalingManager::new(
            service_name.to_string(),
            config.clone(),
            codel_metrics,
            config.scaling_policy.clone(),
        )
    };

    loop {
        if !scaling_paused {
            let current_config = match get_config_by_service(&service_name).await {
                Some(cfg) => cfg,
                None => {
                    slog::error!(log, "Service config not found, stopping auto_scale";
                        "service" => service_name.as_str());
                    break;
                }
            };

            // Get instance data with read lock
            let instances = {
                let instance_store = INSTANCE_STORE.get().unwrap();
                let store = instance_store.read().await;
                match store.get(&*service_name) {
                    Some(instances) => instances.clone(),
                    None => {
                        slog::debug!(log, "Service removed, stopping auto_scale";
                            "service" => service_name.as_str());
                        break;
                    }
                }
            };

            // Collect stats with timeout protection
            let mut pod_stats = HashMap::new();
            let mut missing_containers = Vec::new();

            for (&uuid, metadata) in &instances {
                let mut container_stats = Vec::new();
                let mut pod_failed = false;

                for container in &metadata.containers {
                    match tokio::time::timeout(
                        Duration::from_millis(500),
                        runtime.inspect_container(&container.name),
                    )
                    .await
                    {
                        Ok(Ok(stats)) => {
                            container_stats.push((uuid, metadata.clone(), stats));
                        }
                        Ok(Err(e)) => {
                            if e.to_string().contains("404")
                                || e.to_string().contains("No such container")
                            {
                                pod_failed = true;
                                missing_containers.push((uuid, metadata.clone()));
                                break;
                            }
                        }
                        Err(_) => {
                            slog::warn!(log, "Container inspection timed out";
                                "service" => service_name.as_str(),
                                "container" => &container.name
                            );
                        }
                    }
                }

                if !pod_failed && !container_stats.is_empty() {
                    let strategy = current_config
                        .resource_thresholds
                        .as_ref()
                        .map(|t| &t.metrics_strategy)
                        .unwrap_or(&PodMetricsStrategy::Maximum);

                    let aggregated_stats = aggregate_pod_stats(&container_stats, strategy);
                    pod_stats.insert(uuid, aggregated_stats);
                }
            }

            // Make scaling decision with timeout protection
            match tokio::time::timeout(
                Duration::from_secs(1),
                scaling_manager.evaluate(instances.len(), &pod_stats),
            )
            .await
            {
                Ok(ScalingDecision::ScaleUp(n)) => {
                    slog::info!(log, "Scaling up service";
                        "service" => service_name.as_str(),
                        "instances" => n,
                        "current_instances" => instances.len()
                    );

                    for _ in 0..n {
                        if let Err(e) =
                            scale_up(&service_name, current_config.clone(), runtime.clone()).await
                        {
                            slog::error!(log, "Failed to scale up service";
                                "service" => service_name.as_str(),
                                "error" => e.to_string()
                            );
                            break;
                        }
                    }

                    run_proxy_for_service(service_name.to_string(), current_config.clone()).await;
                }
                Ok(ScalingDecision::ScaleDown(n)) => {
                    let current_count = instances.len();
                    let min_count = current_config.instance_count.min as usize;

                    if current_count <= min_count {
                        slog::debug!(log, "Already at minimum instance count";
                            "service" => service_name.as_str(),
                            "current" => current_count,
                            "min" => min_count
                        );
                        continue;
                    }

                    let scale_down_count = (current_count - min_count).min(n as usize);

                    if scale_down_count > 0 {
                        slog::info!(log, "Scaling down service";
                            "service" => service_name.as_str(),
                            "scale_down_count" => scale_down_count,
                            "current_instances" => current_count
                        );

                        // Find pods with lowest utilization
                        let mut pods: Vec<_> = pod_stats.iter().collect();
                        pods.sort_by(|a, b| {
                            a.1.cpu_percentage.partial_cmp(&b.1.cpu_percentage).unwrap()
                        });

                        let mut scaled_down = 0;
                        for (uuid, _) in pods.iter().take(scale_down_count) {
                            if let Err(e) = scale_down(
                                &service_name,
                                **uuid,
                                current_config.clone(),
                                runtime.clone(),
                            )
                            .await
                            {
                                slog::error!(log, "Failed to scale down service";
                                    "service" => service_name.as_str(),
                                    "error" => e.to_string()
                                );
                                break;
                            }
                            scaled_down += 1;
                        }

                        if scaled_down > 0 {
                            slog::info!(log, "Scale down completed";
                                "service" => service_name.as_str(),
                                "scaled_down" => scaled_down.to_string()
                            );

                            run_proxy_for_service(service_name.to_string(), current_config.clone())
                                .await;
                        }
                    }
                }
                Ok(ScalingDecision::NoChange) => {}
                Err(_) => {
                    slog::warn!(log, "Scaling decision timed out";
                        "service" => service_name.as_str()
                    );
                }
            }
        }

        // Handle configuration updates and scaling pause/resume
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                if scaling_paused {
                    slog::debug!(log, "Scaling is paused, skipping iteration";
                        "service" => service_name.as_str());
                }
            }
            Some((name, message)) = rx.recv() => {
                if name == *service_name {
                    match message {
                        ScaleMessage::ConfigUpdate => {
                            scaling_paused = true;
                            slog::debug!(log, "Scaling paused for config update";
                                "service" => service_name.as_str());
                        }
                        ScaleMessage::Resume => {
                            scaling_paused = false;
                            slog::debug!(log, "Scaling resumed";
                                "service" => service_name.as_str());
                        }
                        ScaleMessage::RollingUpdate => {
                            scaling_paused = true;
                            slog::debug!(log, "Scaling paused for rolling update");
                        }
                        ScaleMessage::RollingUpdateComplete => {
                            scaling_paused = false;
                            slog::debug!(log, "Scaling resumed after rolling update");
                        }
                    }
                }
            }
        }
    }
}

pub async fn scale_up(
    service_name: &str,
    config: ServiceConfig,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<()> {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let server_backends = SERVER_BACKENDS.get().unwrap();

    // Check current instance count with read lock
    let current_instances = {
        let store = instance_store.read().await;
        store
            .get(service_name)
            .map(|instances| instances.len())
            .unwrap_or(0)
    };

    if current_instances >= config.instance_count.max as usize {
        return Ok(());
    }

    let pod_number = get_next_pod_number(service_name).await;

    let started_containers = runtime
        .start_containers(service_name, pod_number, &config.spec.containers, &config)
        .await?;

    // Initialize health monitoring for new containers
    for (container_name, _, _) in &started_containers {
        if let Ok(parts) = parse_container_name(container_name) {
            if let Some(container_config) = config
                .spec
                .containers
                .iter()
                .find(|c| c.name == parts.container_name)
            {
                slog::debug!(log, "Initializing health monitoring for scaled container";
                    "service" => service_name,
                    "container" => container_name,
                    "config_name" => &container_config.name
                );
                if let Err(e) = health::initialize_health_monitoring(
                    service_name,
                    container_name,
                    container_config.health_check.clone(),
                )
                .await
                {
                    slog::error!(log, "Failed to initialize health monitoring";
                        "service" => service_name,
                        "container" => container_name,
                        "error" => e.to_string()
                    );
                }
            }
        }
    }

    let container_parts = parse_container_name(&started_containers[0].0)?;
    let uuid = container_parts.uuid;
    let network_name = format!("{}__{}", service_name, uuid);

    // Get image hashes
    let mut image_hashes = HashMap::new();
    for container in &config.spec.containers {
        if let Ok(hash) = runtime.get_image_digest(&container.image).await {
            image_hashes.insert(container.name.clone(), hash);
        }
    }

    // Update instance store with write lock
    {
        let mut store = instance_store.write().await;
        let service_instances = store
            .entry(service_name.to_string())
            .or_insert_with(FxHashMap::default);

        service_instances.insert(
            uuid,
            InstanceMetadata {
                uuid,
                created_at: SystemTime::now(),
                network: network_name.clone(),
                image_hash: image_hashes,
                containers: started_containers
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

    // Add containers with node_ports to load balancer
    for (container_name, ip, port_metadata) in started_containers {
        for port_info in port_metadata {
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
                        slog::info!(log, "Added backend to load balancer";
                            "service" => service_name,
                            "container" => &container_name,
                            "ip" => &ip,
                            "port" => port_info.port,
                            "node_port" => node_port
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

pub async fn scale_down(
    service_name: &str,
    target_uuid: Uuid,
    config: ServiceConfig,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<()> {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let server_backends = SERVER_BACKENDS.get().unwrap();

    // Check current instances and get target metadata with read lock
    let (current_count, target_metadata) = {
        let store = instance_store.read().await;
        match store.get(service_name) {
            Some(instances) => {
                let count = instances.len();
                let metadata = instances.get(&target_uuid).cloned();
                (count, metadata)
            }
            None => return Ok(()),
        }
    };

    if current_count <= config.instance_count.min as usize {
        return Ok(());
    }

    let target_metadata = match target_metadata {
        Some(metadata) => metadata,
        None => return Ok(()),
    };

    // Remove health monitoring
    if let Some(health_store) = CONTAINER_HEALTH.get() {
        let mut health_map = health_store.write().await;
        // Clone the containers vector to avoid ownership issues
        let containers = target_metadata.containers.clone();
        for container in containers {
            health_map.remove(&container.name);
            slog::debug!(log, "Removed health monitoring";
                "service" => service_name,
                "container" => &container.name
            );
        }
    }

    // Remove from load balancer
    for container in &target_metadata.containers {
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
                        slog::debug!(log, "Removed backend from load balancer";
                            "service" => service_name,
                            "container" => &container.name,
                            "ip" => &container.ip_address,
                            "port" => port_info.port,
                            "node_port" => node_port
                        );
                    }
                }
            }
        }
    }

    // Wait for in-flight requests
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Remove from instance store with write lock
    {
        let mut store = instance_store.write().await;
        if let Some(instances) = store.get_mut(service_name) {
            instances.remove(&target_uuid);
        }
    }

    // Stop containers
    for container in &target_metadata.containers {
        if let Err(e) = runtime.stop_container(&container.name).await {
            slog::error!(log, "Failed to stop container";
                "service" => service_name,
                "container" => &container.name,
                "error" => e.to_string()
            );
            continue;
        }
    }

    // Only try to remove network if it's not a defined network from config
    if config.network.is_none() {
        if let Err(e) = runtime
            .remove_pod_network(&target_metadata.network, service_name)
            .await
        {
            slog::error!(log, "Failed to remove network";
                "service" => service_name,
                "network" => &target_metadata.network,
                "error" => e.to_string()
            );
        }
    }

    Ok(())
}
