// scale.rs
use anyhow::Result;
use pingora_load_balancing::Backend;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    config::{ServiceConfig, CONFIG_UPDATES},
    container::{ContainerRuntime, InstanceMetadata, INSTANCE_STORE, RUNTIME},
    proxy::{run_proxy_for_service, SERVER_BACKENDS},
};

pub async fn auto_scale(service_name: String, initial_config: ServiceConfig) {
    let log = slog_scope::logger();

    // Add initial delay to allow containers to stabilize
    tokio::time::sleep(Duration::from_secs(30)).await;

    let instance_store = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().unwrap().clone();

    // Channel for config updates
    let (tx, mut rx) = mpsc::channel(100);
    CONFIG_UPDATES.get_or_init(|| tx);

    let mut current_config = initial_config;
    let service_name = Arc::new(service_name);

    loop {
        // Check for config updates without holding locks
        if let Ok((_name, new_config)) = rx.try_recv() {
            current_config = new_config;
            slog::debug!(log, "Updated configuration";
                "service" => service_name.as_str(),
                "new_interval" => current_config.interval_seconds.unwrap_or(30)
            );
        }

        // Get current instances safely
        let instances = match instance_store.get(&*service_name) {
            Some(entry) => entry.value().clone(),
            None => {
                slog::debug!(log, "Service removed, stopping auto_scale"; 
                    "service" => service_name.as_str());
                break;
            }
        };

        // Collect stats for all instances in parallel
        let mut instance_stats = Vec::new();
        let mut missing_containers = Vec::new();
        let mut stats_tasks = Vec::new();

        for (&uuid, metadata) in &instances {
            let container_name = format!("{}__{}", service_name, uuid);
            let runtime = runtime.clone();
            let metadata = metadata.clone();
            let service_name = service_name.clone();

            let stats_task = tokio::spawn(async move {
                match runtime.inspect_container(&container_name).await {
                    Ok(stats) => Ok((uuid, metadata, stats)),
                    Err(e) => {
                        if e.to_string().contains("404")
                            || e.to_string().contains("No such container")
                        {
                            Err((uuid, metadata, true))
                        } else {
                            // Other errors we might want to retry
                            slog::warn!(slog_scope::logger(), "Failed to get container stats - will retry next cycle";
                                "service" => service_name.as_str(),
                                "container" => &container_name,
                                "error" => e.to_string()
                            );
                            Err((uuid, metadata, false))
                        }
                    }
                }
            });
            stats_tasks.push(stats_task);
        }

        // Process results with timeout
        match tokio::time::timeout(
            Duration::from_secs(5),
            futures::future::join_all(stats_tasks),
        )
        .await
        {
            Ok(results) => {
                for task_result in results {
                    match task_result {
                        Ok(Ok((uuid, metadata, stats))) => {
                            instance_stats.push((uuid, metadata, stats));
                        }
                        Ok(Err((uuid, metadata, is_missing))) if is_missing => {
                            // Container is confirmed missing - add to cleanup list
                            missing_containers.push((uuid, metadata));
                        }
                        Ok(Err(_)) => {} // Already logged in the task
                        Err(e) => {
                            slog::error!(log, "Task failed";
                                "service" => service_name.as_str(),
                                "error" => %e
                            );
                        }
                    }
                }
            }
            Err(_) => {
                slog::warn!(log, "Stats collection timed out";
                    "service" => service_name.as_str()
                );
                continue; // Skip this cycle
            }
        }

        // Clean up missing containers
        if !missing_containers.is_empty() {
            slog::info!(log, "Cleaning up missing containers";
                "service" => service_name.as_str(),
                "count" => missing_containers.len()
            );

            // Remove from instance store
            if let Some(mut store) = instance_store.get_mut(&*service_name) {
                for (uuid, _) in &missing_containers {
                    store.remove(uuid);
                }
            }

            // Remove from backends
            if let Some(backends) = SERVER_BACKENDS.get().unwrap().get(service_name.as_str()) {
                for (_, metadata) in &missing_containers {
                    let addr = format!("127.0.0.1:{}", metadata.exposed_port);
                    if let Ok(backend) = Backend::new(&addr) {
                        backends.remove(&backend);
                    }
                }
            }
        }

        slog::debug!(log, "Processing resource thresholds";
            "service" => service_name.as_str(),
            "instance_stats_count" => instance_stats.len()
        );

        let instances_exceeding = if let Some(thresholds) = &current_config.resource_thresholds {
            // Collect exceeded thresholds in a single pass
            let mut exceeded_count = 0;
            for (_, _, stats) in &instance_stats {
                // Only consider CPU metrics if they show significant usage
                let cpu_exceeded = thresholds
                    .cpu_percentage
                    .map(|threshold| {
                        stats.cpu_percentage >= 5.0 && // Minimum CPU threshold
                        stats.cpu_percentage > threshold as f64
                    })
                    .unwrap_or(false);

                let cpu_relative_exceeded = thresholds
                    .cpu_percentage_relative
                    .map(|threshold| {
                        stats.cpu_percentage_relative >= 5.0 && // Minimum relative CPU threshold
                        stats.cpu_percentage_relative > threshold as f64
                    })
                    .unwrap_or(false);

                let memory_exceeded = thresholds
                    .memory_percentage
                    .map(|threshold| {
                        let memory_percentage = if stats.memory_limit > 0 {
                            (stats.memory_usage as f64 / stats.memory_limit as f64 * 100.0)
                        } else {
                            0.0
                        };
                        memory_percentage > threshold as f64 && memory_percentage >= 5.0
                    })
                    .unwrap_or(false);

                let should_exceed = cpu_exceeded || cpu_relative_exceeded || memory_exceeded;

                if should_exceed {
                    exceeded_count += 1;
                    slog::debug!(log, "Container exceeding thresholds";
                        "service" => service_name.as_str(),
                        "cpu_percentage" => stats.cpu_percentage,
                        "cpu_percentage_relative" => stats.cpu_percentage_relative,
                        "memory_percentage" => if stats.memory_limit > 0 {
                            (stats.memory_usage as f64 / stats.memory_limit as f64 * 100.0)
                        } else {
                            0.0
                        }
                    );
                }
            }

            // Log resource check once per cycle
            slog::debug!(log, "Resource threshold check";
                "service" => service_name.as_str(),
                "exceeding_count" => exceeded_count.to_string(),
                "total_instances" => instance_stats.len().to_string(),
                "cpu_threshold" => thresholds.cpu_percentage,
                "cpu_relative_threshold" => thresholds.cpu_percentage_relative,
                "memory_threshold" => thresholds.memory_percentage
            );

            exceeded_count
        } else {
            0
        };

        // Scale up if needed
        if ((current_config.resource_thresholds.is_some() && instances_exceeding > 0)
            && instances.len() < current_config.instance_count.max as usize)
            || (instances.len() < current_config.instance_count.min as usize)
        {
            slog::debug!(log, "Scaling up service";
                "service" => service_name.as_str(),
                "current_instances" => instances.len(),
                "instances_exceeding" => instances_exceeding.to_string()
            );

            if let Err(e) = scale_up(&service_name, current_config.clone(), runtime.clone()).await {
                slog::error!(log, "Failed to scale up service";
                    "service" => service_name.as_str(),
                    "error" => e.to_string()
                );
            }

            // Update proxy configuration after scale up
            run_proxy_for_service(service_name.to_string(), current_config.clone()).await;
        }
        // Scale down if needed
        else if (current_config.resource_thresholds.is_none() || instances_exceeding == 0)
            && instances.len() > current_config.instance_count.min as usize
        {
            if let Some((uuid, _, _)) =
                instance_stats
                    .iter()
                    .min_by(|(_, _, stats_a), (_, _, stats_b)| {
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

                if let Err(e) = scale_down(
                    &service_name,
                    *uuid,
                    current_config.clone(),
                    runtime.clone(),
                )
                .await
                {
                    slog::error!(log, "Failed to scale down service";
                        "service" => service_name.as_str(),
                        "error" => e.to_string()
                    );
                }

                // Update proxy configuration after scale down
                run_proxy_for_service(service_name.to_string(), current_config.clone()).await;
            }
        }

        // Sleep with timeout
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(current_config.interval_seconds.unwrap_or(30))) => {},
            _ = rx.recv() => {
                // Config update received, skip sleep and process immediately
                continue;
            }
        }
    }
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

    // Get instance data without holding long-term lock
    let instance_data = match instance_store.get(service_name) {
        Some(entry) => entry.value().clone(),
        None => return Ok(()),
    };

    if instance_data.len() <= config.instance_count.min as usize {
        return Ok(());
    }

    // Get target instance metadata
    let target_metadata = match instance_data.get(&target_uuid) {
        Some(metadata) => metadata.clone(),
        None => return Ok(()),
    };

    let container_name = format!("{}__{}", service_name, target_uuid);
    let exposed_port = target_metadata.exposed_port;

    // Step 1: Remove from load balancer first
    if let Some(backends) = server_backends.get(service_name) {
        let addr = format!("127.0.0.1:{}", exposed_port);
        backends.remove(&Backend::new(&addr).unwrap());
        slog::trace!(log, "Removed backend from load balancer";
            "service" => service_name,
            "container" => container_name.clone(),
            "address" => addr
        );
    }

    // Step 2: Wait for in-flight requests to complete
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Step 3: Stop the container
    if let Err(e) = runtime.stop_container(&container_name).await {
        slog::error!(log, "Failed to stop container";
            "service" => service_name,
            "container" => &container_name,
            "error" => e.to_string()
        );
        return Err(anyhow::anyhow!("Failed to stop container: {}", e));
    }

    // Step 4: Remove from instance store
    if let Some(mut instances) = instance_store.get_mut(service_name) {
        instances.remove(&target_uuid);
    }

    slog::debug!(log, "Successfully scaled down container";
        "service" => service_name,
        "container" => container_name
    );

    Ok(())
}

pub async fn scale_up(
    service_name: &str,
    config: ServiceConfig,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<()> {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();

    // Get current instances without holding long-term lock
    let current_instances = match instance_store.get(service_name) {
        Some(entry) => entry.value().len(),
        None => 0,
    };

    if current_instances >= config.instance_count.max as usize {
        return Ok(());
    }

    let uuid = Uuid::new_v4();
    let container_name = format!("{}__{}", service_name, uuid);
    let port_range = config.exposed_port..config.exposed_port + 10;

    // Step 1: Start the container
    let exposed_port = runtime
        .start_container(
            &container_name,
            &config.image,
            config.target_port,
            port_range,
            config.memory_limit.clone(),
            config.cpu_limit.clone(),
        )
        .await?;

    // Step 2: Add to instance store
    if let Some(mut instances) = instance_store.get_mut(service_name) {
        instances.insert(
            uuid,
            InstanceMetadata {
                uuid,
                exposed_port,
                status: "running".to_string(),
            },
        );
    }

    slog::debug!(log, "Successfully scaled up container";
        "service" => service_name,
        "container" => container_name
    );

    Ok(())
}
