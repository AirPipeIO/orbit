// scale.rs
use anyhow::Result;
use pingora_load_balancing::Backend;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    config::{get_config_by_service, ScaleMessage, ServiceConfig, CONFIG_UPDATES},
    container::{ContainerRuntime, InstanceMetadata, INSTANCE_STORE, RUNTIME},
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
            let mut instance_stats = Vec::new();
            let mut missing_containers = Vec::new();

            // Process containers in a single batch
            for (&uuid, metadata) in &instances {
                let container_name = format!("{}__{}", service_name, uuid);
                match runtime.inspect_container(&container_name).await {
                    Ok(stats) => {
                        instance_stats.push((uuid, metadata.clone(), stats));
                    }
                    Err(e) => {
                        if e.to_string().contains("404")
                            || e.to_string().contains("No such container")
                        {
                            missing_containers.push((uuid, metadata.clone()));

                            // Remove from load balancer
                            if let Some(backends) =
                                SERVER_BACKENDS.get().unwrap().get(&*service_name)
                            {
                                let addr = format!("127.0.0.1:{}", metadata.exposed_port);
                                if let Ok(backend) = Backend::new(&addr) {
                                    backends.remove(&backend);
                                    slog::info!(log, "Removed missing container from load balancer";
                                        "service" => service_name.as_str(),
                                        "container" => &container_name,
                                        "address" => addr
                                    );
                                }
                            }

                            // Clean up stats separately after we're done with inspection
                            // cleanup_container_stats(&container_name);
                        }
                    }
                }
            }

            // Handle missing containers without holding long-term locks
            if !missing_containers.is_empty() {
                // Remove missing containers from instance store
                use dashmap::try_result::TryResult;
                if let TryResult::Present(mut instances_entry) =
                    instance_store.try_get_mut(&*service_name)
                {
                    for (uuid, _) in &missing_containers {
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
                            scale_up(&service_name, current_config.clone(), runtime.clone()).await
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

            slog::debug!(log, "Completed collecting instance stats";
    "service" => service_name.as_str(),
    "stats_count" => instance_stats.len());

            let should_scale = if let Some(thresholds) = &current_config.resource_thresholds {
                slog::debug!(log, "Starting threshold evaluation";
        "service" => service_name.as_str(),
        "config_address" => format!("{:p}", &current_config),
        "threshold_address" => format!("{:p}", thresholds),
        "cpu_relative" => thresholds.cpu_percentage_relative);

                // Count how many instances exceed thresholds
                let mut instances_exceeding = 0;
                let mut total_instances = 0;

                for (_, _, stats) in &instance_stats {
                    total_instances += 1;
                    let memory_percentage = if stats.memory_limit > 0 {
                        (stats.memory_usage as f64 / stats.memory_limit as f64 * 100.0)
                    } else {
                        0.0
                    };

                    let cpu_exceeded = thresholds
                        .cpu_percentage
                        .map(|threshold| {
                            stats.cpu_percentage >= 5.0 && stats.cpu_percentage > threshold as f64
                        })
                        .unwrap_or(false);

                    // let cpu_relative_exceeded = thresholds
                    //     .cpu_percentage_relative
                    //     .map(|threshold| {
                    //         stats.cpu_percentage_relative >= 5.0
                    //             && stats.cpu_percentage_relative > threshold as f64
                    //     })
                    //     .unwrap_or(false);

                    let cpu_relative_exceeded = match thresholds.cpu_percentage_relative {
                        Some(threshold) => {
                            stats.cpu_percentage_relative >= 5.0
                                && stats.cpu_percentage_relative > threshold as f64
                        }
                        None => false,
                    };

                    // Log the actual check
                    slog::debug!(log, "Individual threshold check";
                        "service" => service_name.as_str(),
                        "cpu_relative_exists" => thresholds.cpu_percentage_relative.is_some(),
                        "cpu_relative_value" => stats.cpu_percentage_relative,
                        "exceeded" => cpu_relative_exceeded);

                    let memory_exceeded = thresholds
                        .memory_percentage
                        .map(|threshold| {
                            memory_percentage > threshold as f64 && memory_percentage >= 5.0
                        })
                        .unwrap_or(false);

                    // Log metrics for each container
                    // slog::info!(log, "Container metrics";
                    //     "service" => service_name.as_str(),
                    //     "container_id" => &stats.id,
                    //     "cpu_percentage" => stats.cpu_percentage,
                    //     "cpu_percentage_relative" => stats.cpu_percentage_relative,
                    //     "memory_percentage" => memory_percentage,
                    //     "memory_usage_mb" => stats.memory_usage as f64 / 1024.0 / 1024.0,
                    //     "memory_limit_mb" => stats.memory_limit as f64 / 1024.0 / 1024.0,
                    //     "thresholds_exceeded" => format!("cpu={} cpu_rel={} mem={}",
                    //         cpu_exceeded,
                    //         cpu_relative_exceeded,
                    //         memory_exceeded
                    //     )
                    // );

                    if cpu_exceeded || cpu_relative_exceeded || memory_exceeded {
                        instances_exceeding += 1;
                    }
                }

                // Calculate the percentage of instances that are exceeding thresholds
                let percentage_exceeding = if total_instances > 0 {
                    (f64::from(instances_exceeding) / f64::from(total_instances)) * 100.0
                } else {
                    0.0
                };

                // Scale if at least 75% of instances are exceeding thresholds
                let should_scale = percentage_exceeding >= 75.0;

                if should_scale {
                    slog::info!(log, "High load detected across instances";
                        "service" => service_name.as_str(),
                        "instances_exceeding" => instances_exceeding.to_string(),
                        "total_instances" => total_instances.to_string(),
                        "percentage_exceeding" => percentage_exceeding.to_string()
                    );
                }

                should_scale
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

    // Get current instances without holding long-term lock
    let current_instances = match instance_store.get(service_name) {
        Some(entry) => entry.value().len(),
        None => 0,
    };

    if current_instances >= config.instance_count.max as usize {
        return Ok(());
    }

    let uuid = uuid::Uuid::new_v4();
    let container_name = format!("{}__{}", service_name, uuid);

    // Step 1: Start the container
    let exposed_port = runtime
        .start_container(
            &container_name,
            &config.image,
            config.target_port,
            config.memory_limit.clone(),
            config.cpu_limit.clone(),
        )
        .await?;

    slog::info!(log, "Container started";
        "service" => service_name,
        "container" => &container_name,
        "port" => exposed_port
    );

    // Step 2: Wait for container to be healthy (implement health check)
    let healthy = wait_for_container_health(&container_name, runtime.clone()).await;

    if !healthy {
        slog::error!(log, "Container failed health check, removing";
            "service" => service_name,
            "container" => &container_name
        );
        let _ = runtime.stop_container(&container_name).await;
        return Err(anyhow::anyhow!("Container failed health check"));
    }

    // Step 3: Add to instance store
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

    // Step 4: Add to load balancer after container is healthy
    if let Some(backends) = server_backends.get(service_name) {
        let addr = format!("127.0.0.1:{}", exposed_port);
        if let Ok(backend) = Backend::new(&addr) {
            backends.insert(backend);
            slog::info!(log, "Added backend to load balancer";
                "service" => service_name,
                "container" => &container_name,
                "address" => &addr
            );
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
    let addr = format!("127.0.0.1:{}", exposed_port);

    // Step 1: Remove from load balancer first
    if let Some(backends) = server_backends.get(service_name) {
        if let Ok(backend) = Backend::new(&addr) {
            backends.remove(&backend);
            slog::info!(log, "Removed backend from load balancer";
                "service" => service_name,
                "container" => &container_name,
                "address" => &addr
            );
        }
    }

    // Step 2: Wait for in-flight requests to complete
    slog::info!(log, "Waiting for in-flight requests";
        "service" => service_name,
        "container" => &container_name
    );
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Step 3: Remove from instance store
    if let Some(mut instances) = instance_store.get_mut(service_name) {
        instances.remove(&target_uuid);
    }

    // Step 4: Stop the container
    if let Err(e) = runtime.stop_container(&container_name).await {
        slog::error!(log, "Failed to stop container";
            "service" => service_name,
            "container" => &container_name,
            "error" => e.to_string()
        );
        return Err(anyhow::anyhow!("Failed to stop container: {}", e));
    }

    slog::info!(log, "Successfully scaled down container";
        "service" => service_name,
        "container" => &container_name
    );

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
