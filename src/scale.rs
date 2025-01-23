// scale.rs
use anyhow::{anyhow, Result};
use pingora_load_balancing::Backend;
use std::{collections::HashMap, sync::Arc, time::Duration};
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

    // Initialize the service config
    let mut scaling_paused = false;
    let service_name = Arc::new(service_name);

    loop {
        // Only perform scaling operations if not paused
        if !scaling_paused {
            // Get latest config from shared store
            let current_config = match get_config_by_service(&service_name) {
                Some(cfg) => cfg,
                None => {
                    slog::error!(log, "Service config not found, stopping auto_scale";
                    "service" => service_name.as_str());
                    break;
                }
            };

            // Log the current thresholds configuration
            slog::debug!(log, "Current scaling configuration";
                "service" => service_name.as_str(),
                "thresholds" => format!("{:?}", current_config.resource_thresholds)
            );

            // Only proceed with threshold evaluation if thresholds are configured
            if let Some(_thresholds) = &current_config.resource_thresholds {
                // Process resource thresholds only once per iteration
                let instances = match instance_store.get(&*service_name) {
                    Some(entry) => entry.value().clone(),
                    None => {
                        slog::debug!(log, "Service removed, stopping auto_scale"; 
                        "service" => service_name.as_str());
                        break;
                    }
                };

                // Collect stats for all instances
                let mut missing_containers = Vec::new();

                let mut pod_stats = HashMap::new();

                // Process containers in a single batch
                for (&uuid, metadata) in &instances {
                    let mut container_stats = Vec::new();
                    let mut pod_failed = false;

                    // Collect stats for all containers in the pod
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
                                    // Remove all pod's containers from load balancer
                                    for container in &metadata.containers {
                                        for port_info in &container.ports {
                                            if let Some(node_port) = port_info.node_port {
                                                let proxy_key =
                                                    format!("{}_{}", service_name, node_port);
                                                if let Some(backends) =
                                                    SERVER_BACKENDS.get().unwrap().get(&proxy_key)
                                                {
                                                    let addr = format!(
                                                        "{}:{}",
                                                        container.ip_address, port_info.port
                                                    );
                                                    if let Ok(backend) = Backend::new(&addr) {
                                                        backends.remove(&backend);
                                                        slog::debug!(log, "Removed missing container backend";
                                                            "service" => %service_name,
                                                            "container" => &container.name,
                                                            "container_port" => port_info.port,
                                                            "node_port" => node_port,
                                                            "address" => &addr
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    break; // Exit container loop as pod has failed
                                }
                            }
                        }
                    }

                    if !pod_failed && !container_stats.is_empty() {
                        let strategy = &current_config
                            .resource_thresholds
                            .as_ref()
                            .map(|t| &t.metrics_strategy)
                            .unwrap_or(&PodMetricsStrategy::Maximum);

                        let aggregated_stats = aggregate_pod_stats(&container_stats, strategy);
                        pod_stats.insert(uuid, aggregated_stats);
                    }
                }

                // Handle missing containers without holding long-term locks
                if !missing_containers.is_empty() {
                    // Remove missing containers from instance store
                    use dashmap::try_result::TryResult;
                    if let TryResult::Present(mut instances_entry) =
                        instance_store.try_get_mut(&*service_name)
                    {
                        for (uuid, metadata) in &missing_containers {
                            // Cleanup all containers in the pod, even if only one failed
                            for container in &metadata.containers {
                                if let Err(e) = runtime.stop_container(&container.name).await {
                                    slog::error!(log, "Failed to stop container in incomplete pod";
                                        "service" => service_name.clone(),
                                        "container" => &container.name,
                                        "error" => e.to_string()
                                    );
                                }
                            }

                            // Remove pod network
                            if let Err(e) = runtime.remove_pod_network(&metadata.network).await {
                                slog::error!(log, "Failed to remove network for incomplete pod";
                                    "service" => service_name.clone(),
                                    "network" => &metadata.network,
                                    "error" => e.to_string()
                                );
                            }

                            // Remove from instance store
                            instances_entry.remove(uuid);
                        }

                        // Check if we need to replace containers
                        let current_count = instances_entry.len();
                        drop(instances_entry); // Release the lock before scaling

                        if current_count < current_config.instance_count.min as usize {
                            slog::info!(log, "Replacing terminated containers to maintain minimum count";
                                "service" => service_name.as_str(),
                                "current_count" => current_count,
                                "min_count" => current_config.instance_count.min
                            );

                            if let Err(e) =
                                scale_up(&service_name, current_config.clone(), runtime.clone())
                                    .await
                            {
                                slog::error!(log, "Failed to replace terminated containers";
                                    "service" => service_name.as_str(),
                                    "error" => e.to_string()
                                );
                            }

                            // Update proxy configuration after scaling up
                            run_proxy_for_service(service_name.to_string(), current_config.clone())
                                .await;
                        }
                    }
                }
                let should_scale = if let Some(thresholds) = &current_config.resource_thresholds {
                    slog::debug!(log, "Starting threshold evaluation";
                        "service" => service_name.as_str(),
                        "config_address" => format!("{:p}", &current_config),
                        "threshold_address" => format!("{:p}", thresholds),
                        "cpu_relative" => thresholds.cpu_percentage_relative,
                        "cpu_absolute" => thresholds.cpu_percentage,
                        "memory_absolute" => thresholds.memory_percentage
                    );

                    let mut pods_exceeding = 0;
                    let mut total_evaluated_pods = 0; // Count only pods with ready metrics
                    let mut skipped_pods_due_to_unready_metrics = 0;

                    for (_, stats) in &pod_stats {
                        let memory_percentage = if stats.memory_limit > 0 {
                            (stats.memory_usage as f64 / stats.memory_limit as f64 * 100.0)
                        } else {
                            0.0
                        };

                        let cpu_ready = stats.cpu_percentage > 0.0;
                        let cpu_relative_ready = stats.cpu_percentage_relative > 0.0;

                        if !cpu_ready || !cpu_relative_ready {
                            skipped_pods_due_to_unready_metrics += 1;
                            slog::debug!(log, "Skipping pod with unready metrics";
                                "service" => service_name.as_str(),
                                "cpu_ready" => cpu_ready,
                                "cpu_relative_ready" => cpu_relative_ready,
                                "cpu_value" => stats.cpu_percentage,
                                "cpu_relative_value" => stats.cpu_percentage_relative
                            );
                            continue; // Skip this pod entirely
                        }

                        // If the pod is ready, include it in the evaluation
                        total_evaluated_pods += 1;

                        let cpu_exceeded = thresholds.cpu_percentage.map_or(false, |threshold| {
                            stats.cpu_percentage >= 5.0 && stats.cpu_percentage > threshold as f64
                        });

                        let cpu_relative_exceeded =
                            thresholds
                                .cpu_percentage_relative
                                .map_or(false, |threshold| {
                                    stats.cpu_percentage_relative >= 5.0
                                        && stats.cpu_percentage_relative > threshold as f64
                                });

                        let memory_exceeded =
                            thresholds.memory_percentage.map_or(false, |threshold| {
                                memory_percentage >= 5.0 && memory_percentage > threshold as f64
                            });

                        // Log the evaluation details for each pod
                        slog::debug!(log, "Pod threshold evaluation";
                            "service" => service_name.as_str(),
                            "cpu_absolute_threshold_exists" => thresholds.cpu_percentage.is_some(),
                            "cpu_absolute_value" => stats.cpu_percentage,
                            "cpu_absolute_exceeded" => cpu_exceeded,
                            "cpu_relative_threshold_exists" => thresholds.cpu_percentage_relative.is_some(),
                            "cpu_relative_value" => stats.cpu_percentage_relative,
                            "cpu_relative_exceeded" => cpu_relative_exceeded,
                            "memory_threshold_exists" => thresholds.memory_percentage.is_some(),
                            "memory_value" => memory_percentage,
                            "memory_exceeded" => memory_exceeded
                        );

                        if cpu_exceeded || cpu_relative_exceeded || memory_exceeded {
                            pods_exceeding += 1;
                        }
                    }

                    // Calculate the percentage of pods that are exceeding thresholds
                    let percentage_exceeding = if total_evaluated_pods > 0 {
                        (pods_exceeding as f64 / total_evaluated_pods as f64) * 100.0
                    } else {
                        0.0
                    };

                    slog::info!(log, "Threshold evaluation summary";
                        "service" => service_name.as_str(),
                        "pods_exceeding" => pods_exceeding.to_string(),
                        "total_pods" => pod_stats.len(), // Original total pods before skipping
                        "total_evaluated_pods" => total_evaluated_pods.to_string(), // Pods with ready metrics
                        "percentage_exceeding" => percentage_exceeding.to_string(),
                        "skipped_pods_due_to_unready_metrics" => skipped_pods_due_to_unready_metrics.to_string(),
                        "should_scale" => percentage_exceeding >= 75.0
                    );

                    percentage_exceeding >= 75.0
                } else {
                    false
                };

                slog::debug!(log, "Completed threshold check";
    "service" => service_name.as_str(),
    "should_scale" => should_scale);

                // Scale up if needed
                if ((current_config.resource_thresholds.is_some() && should_scale)
                    && instances.len() < current_config.instance_count.max as usize)
                    || (instances.len() < current_config.instance_count.min as usize)
                {
                    slog::debug!(log, "Scaling up service";
                        "service" => service_name.as_str(),
                        "current_instances" => instances.len(),
                        // "instances_exceeding" => instances_exceeding.to_string()
                    );

                    if let Err(e) =
                        scale_up(&service_name, current_config.clone(), runtime.clone()).await
                    {
                        slog::error!(log, "Failed to scale up service";
                            "service" => service_name.as_str(),
                            "error" => e.to_string()
                        );
                    }

                    // Update proxy configuration after scale up
                    run_proxy_for_service(service_name.to_string(), current_config.clone()).await;
                }
                // Scale down if needed
                else if (current_config.resource_thresholds.is_none() || !should_scale)
                    && instances.len() > current_config.instance_count.min as usize
                {
                    if let Some((&uuid, _)) =
                        pod_stats.iter().min_by(|(_, stats_a), (_, stats_b)| {
                            stats_a
                                .cpu_percentage
                                .partial_cmp(&stats_b.cpu_percentage)
                                .unwrap()
                        })
                    {
                        slog::debug!(log, "Scaling down service";
                            "service" => service_name.as_str(),
                            "current_instances" => instances.len()
                        );

                        if let Err(e) =
                            scale_down(&service_name, uuid, current_config.clone(), runtime.clone())
                                .await
                        {
                            slog::error!(log, "Failed to scale down service";
                                "service" => service_name.as_str(),
                                "error" => e.to_string()
                            );
                        }

                        // Update proxy configuration after scale down
                        run_proxy_for_service(service_name.to_string(), current_config.clone())
                            .await;
                    }
                }
            }
        } else {
            slog::debug!(log, "No thresholds configured, skipping scaling evaluation";
                "service" => service_name.as_str()
            );
        }

        // Update message handling to be simpler
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
    let network_name = format!("{}_{}", service_name, uuid);

    if let Some(mut instances) = instance_store.get_mut(service_name) {
        instances.insert(
            uuid,
            InstanceMetadata {
                uuid,
                network: network_name.clone(),
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
                let proxy_key = format!("{}_{}", service_name, node_port);
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

    // Remove network
    if let Err(e) = runtime.remove_pod_network(&target_metadata.network).await {
        slog::error!(log, "Failed to remove network";
            "service" => service_name,
            "network" => &target_metadata.network,
            "error" => e.to_string()
        );
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
