// src/container/scaling/mod.rs
pub mod codel;
pub mod manager;
use anyhow::Result;
use codel::get_service_metrics;
use manager::{ScalingDecision, UnifiedScalingManager};
use pingora_load_balancing::Backend;
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
        get_next_pod_number, ContainerMetadata, ContainerRuntime, InstanceMetadata, INSTANCE_STORE,
        RUNTIME,
    },
    proxy::{run_proxy_for_service, SERVER_BACKENDS},
};

pub async fn auto_scale(service_name: String) {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().unwrap().clone();

    // Channel for config updates
    let (tx, mut rx) = mpsc::channel(100);
    CONFIG_UPDATES.get_or_init(|| tx);

    // Initialize the service config and scaling manager
    let mut scaling_paused = false;
    let service_name = Arc::new(service_name);

    // Initialize the scaling manager
    let mut scaling_manager = {
        let config = match get_config_by_service(&service_name) {
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
            config.scaling_policy.clone(), // Pass the user's scaling policy if provided
        )
    };

    loop {
        if !scaling_paused {
            let current_config = match get_config_by_service(&service_name) {
                Some(cfg) => cfg,
                None => {
                    slog::error!(log, "Service config not found, stopping auto_scale";
                        "service" => service_name.as_str());
                    break;
                }
            };

            let instances = match instance_store.get(&*service_name) {
                Some(entry) => entry.value().clone(),
                None => {
                    slog::debug!(log, "Service removed, stopping auto_scale";
                        "service" => service_name.as_str());
                    break;
                }
            };

            // Collect stats for scaling decision
            let mut pod_stats = HashMap::new();
            let mut missing_containers = Vec::new();
            for (&uuid, metadata) in &instances {
                let mut container_stats = Vec::new();
                let mut pod_failed = false;

                for container in &metadata.containers {
                    match runtime.inspect_container(&container.name).await {
                        Ok(stats) => {
                            container_stats.push((uuid, metadata.clone(), stats));
                        }
                        Err(e) => {
                            if e.to_string().contains("404")
                                || e.to_string().contains("No such container")
                            {
                                pod_failed = true;
                                missing_containers.push((uuid, metadata.clone()));
                                break;
                            }
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

            // Get scaling decision
            match scaling_manager.evaluate(instances.len(), &pod_stats).await {
                ScalingDecision::ScaleUp(n) => {
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
                    scaling_manager.enter_cooldown();
                }
                ScalingDecision::ScaleDown(n) => {
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

                    if scale_down_count == 0 {
                        continue;
                    }

                    slog::info!(log, "Scaling down service";
                        "service" => service_name.as_str(),
                        "scale_down_count" => scale_down_count,
                        "current_instances" => current_count,
                        "min_instances" => min_count
                    );

                    // Find pods with lowest utilization
                    let mut pods: Vec<_> = pod_stats.iter().collect();
                    pods.sort_by(|a, b| {
                        a.1.cpu_percentage.partial_cmp(&b.1.cpu_percentage).unwrap()
                    });

                    let mut scaled_down = 0;
                    for _ in 0..scale_down_count {
                        if let Some((&uuid, _)) = pods.first() {
                            if let Err(e) = scale_down(
                                &service_name,
                                uuid,
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
                            pods.remove(0);
                        }
                    }

                    if scaled_down > 0 {
                        slog::info!(log, "Scale down completed";
                            "service" => service_name.as_str(),
                            "scaled_down" => scaled_down.to_string()
                        );

                        run_proxy_for_service(service_name.to_string(), current_config.clone())
                            .await;
                        scaling_manager.enter_cooldown();
                    }
                }
                ScalingDecision::NoChange => {}
            }
        }

        // Handle configuration updates and scaling pause/resume
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                if scaling_paused {
                    slog::debug!(log, "Scaling is paused, skipping iteration";
                        "service" => service_name.as_str());
                }
            },
            Some((name, message)) = rx.recv() => {
                if name == *service_name {
                    match message {
                        ScaleMessage::RollingUpdate => {
                            scaling_paused = true;
                            slog::debug!(log, "Scaling paused for rolling update");
                        }
                        ScaleMessage::RollingUpdateComplete => {
                            scaling_paused = false;
                            slog::debug!(log, "Scaling resumed after rolling update");
                        }
                        ScaleMessage::ConfigUpdate => {
                            scaling_paused = true;
                            slog::debug!(log, "Scaling paused for config update";
                                "service" => service_name.as_str());
                        },
                        ScaleMessage::Resume => {
                            scaling_paused = false;
                            slog::debug!(log, "Scaling resumed";
                                "service" => service_name.as_str());
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

    let current_instances = match instance_store.get(service_name) {
        Some(entry) => entry.value().len(),
        None => 0,
    };

    if current_instances >= config.instance_count.max as usize {
        return Ok(());
    }

    let pod_number = get_next_pod_number(service_name).await;

    let started_containers = runtime
        .start_containers(service_name, pod_number, &config.spec.containers, &config)
        .await?;

    let container_parts = parse_container_name(&started_containers[0].0)?;
    let uuid = container_parts.uuid;
    let network_name = format!("{}__{}", service_name, uuid);

    if let Some(mut instances) = instance_store.get_mut(service_name) {
        let now = SystemTime::now();
        let mut image_hashes = HashMap::new();

        // Get image hashes for containers being started
        for container in &config.spec.containers {
            if let Ok(hash) = runtime.get_image_digest(&container.image).await {
                image_hashes.insert(container.name.clone(), hash);
            }
        }

        instances.insert(
            uuid,
            InstanceMetadata {
                uuid,
                created_at: now,
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
                if let Some(backends) = server_backends.get(&proxy_key) {
                    let addr = format!("{}:{}", ip, port_info.port);
                    if let Ok(backend) = Backend::new(&addr) {
                        backends.insert(backend);
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

    let instance_data = match instance_store.get(service_name) {
        Some(entry) => entry.value().clone(),
        None => return Ok(()),
    };

    if instance_data.len() <= config.instance_count.min as usize {
        return Ok(());
    }

    let target_metadata = match instance_data.get(&target_uuid) {
        Some(metadata) => metadata.clone(),
        None => return Ok(()),
    };

    // Remove containers from load balancer
    for container in &target_metadata.containers {
        for port_info in &container.ports {
            if let Some(node_port) = port_info.node_port {
                let proxy_key = format!("{}__{}", service_name, node_port);
                if let Some(backends) = server_backends.get(&proxy_key) {
                    let addr = format!("{}:{}", container.ip_address, port_info.port);
                    if let Ok(backend) = Backend::new(&addr) {
                        backends.remove(&backend);
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

    // Remove from instance store
    if let Some(mut instances) = instance_store.get_mut(service_name) {
        instances.remove(&target_uuid);
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
        // Only then try to remove the auto-generated network
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

// Health check function
async fn wait_for_container_health(
    container_name: &str,
    runtime: Arc<dyn ContainerRuntime>,
) -> bool {
    let log = slog_scope::logger();

    // Try up to 30 times with 1 second delay (30 seconds total)
    for attempt in 1..=30 {
        match runtime.inspect_container(container_name).await {
            Ok(stats) => {
                slog::debug!(log, "Container health check";
                    "container" => container_name,
                    "attempt" => attempt.to_string(),
                    "cpu" => stats.cpu_percentage,
                    "memory" => stats.memory_usage
                );

                // Container is considered healthy if we can get stats
                return true;
            }
            Err(e) => {
                slog::debug!(log, "Container health check failed";
                    "container" => container_name,
                    "attempt" => attempt.to_string(),
                    "error" => e.to_string()
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    false
}

async fn check_application_readiness(addr: &str, port: u16, timeout: Duration) -> bool {
    use tokio::net::TcpStream;

    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        match tokio::time::timeout(
            Duration::from_secs(1),
            TcpStream::connect(format!("{}:{}", addr, port)),
        )
        .await
        {
            Ok(Ok(_)) => return true, // Successfully connected
            _ => {
                // Only sleep if we still have time remaining
                if start.elapsed() < timeout {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            }
        }
    }
    false
}
