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

    let instance_store: &dashmap::DashMap<
        String,
        std::collections::HashMap<Uuid, InstanceMetadata>,
    > = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().unwrap().clone();

    // Channel for config updates
    let (tx, mut rx) = mpsc::channel(100);
    CONFIG_UPDATES.get_or_init(|| tx);

    let mut current_config = initial_config;
    let service_name = Arc::new(service_name);

    loop {
        // Check for config updates
        if let Ok((_name, new_config)) = rx.try_recv() {
            current_config = new_config;
            slog::debug!(log, "Updated configuration"; 
                "service" => service_name.as_str(), 
                "new_interval" => current_config.interval_seconds.unwrap_or(30),
                "new_cpu_threshold" => current_config.resource_thresholds
                    .as_ref()
                    .and_then(|t| t.cpu_percentage)
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "None".to_string()));
        }

        // Get current instances
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
        for (uuid, metadata) in &instances {
            let container_name = format!("{}__{}", service_name, uuid);
            match runtime.inspect_container(&container_name).await {
                Ok(stats) => {
                    // slog::debug!(log, "Container stats";
                    //     "service" => service_name.as_str(),
                    //     "container" => container_name,
                    //     "cpu_percent" => stats.cpu_percentage,
                    //     "threshold" => current_config.resource_thresholds
                    //         .as_ref()
                    //         .and_then(|t| t.cpu_percentage)
                    //         .map(|t| t.to_string())
                    //         .unwrap_or_else(|| "None".to_string()));
                    instance_stats.push((uuid, metadata.clone(), stats));
                }
                Err(e) => {
                    // If we get no stats we assume the container is unhealthy
                    // Handle non-existent or unhealthy containers
                    slog::warn!(log, "Detected unhealthy or missing container";
                        "service" => service_name.as_str(),
                        "error" => e.to_string(),
                        "container" => &container_name);

                    // Remove from instance store
                    instance_store.get_mut(&*service_name.clone()).map(
                        |mut entry: dashmap::mapref::one::RefMut<
                            '_,
                            String,
                            std::collections::HashMap<Uuid, InstanceMetadata>,
                        >| entry.remove(uuid),
                    );

                    // Remove from upstreams
                    if let Some(backends) = SERVER_BACKENDS
                        .get()
                        .and_then(|sb| sb.get(service_name.as_str()))
                    {
                        let addr = format!("127.0.0.1:{}", metadata.exposed_port);
                        backends.remove(&Backend::new(&addr).unwrap());
                        slog::debug!(log, "Removed backend for unhealthy container";
                                "service" => service_name.as_str(),
                                "container" => container_name);
                    }
                }
            }
        }

        let instances_exceeding = if let Some(thresholds) = &current_config.resource_thresholds {
            instance_stats
                .iter()
                .filter(|(_, _, stats)| {
                    let cpu_exceeded = thresholds
                        .cpu_percentage
                        .map(|threshold| stats.cpu_percentage > threshold as f64)
                        .unwrap_or(false);

                    let cpu_relative_exceeded = thresholds
                        .cpu_percentage_relative
                        .map(|threshold| stats.cpu_percentage_relative > threshold as f64)
                        .unwrap_or(false);

                    let memory_exceeded = thresholds
                        .memory_percentage
                        .map(|threshold| {
                            (stats.memory_usage as f64 / stats.memory_limit as f64 * 100.0)
                                > threshold as f64
                        })
                        .unwrap_or(false);

                    cpu_exceeded || cpu_relative_exceeded || memory_exceeded
                })
                .count()
        } else {
            0 // If no thresholds defined at all, consider no instances as exceeding
        };

        slog::trace!(log, "Scaling check"; 
        "service" => service_name.as_str(),
        "total_instances" => instances.len(),
        "instances_exceeding" => instances_exceeding,
        "thresholds_defined" => current_config.resource_thresholds.is_some(),
        "cpu_threshold" => current_config.resource_thresholds
            .as_ref()
            .and_then(|t| t.cpu_percentage)
            .map(|t| t.to_string())
            .unwrap_or_else(|| "None".to_string()));

        // Scale up if ANY instance is over threshold or if below minimum
        if ((current_config.resource_thresholds.is_some() && instances_exceeding > 0)
            && instances.len() < current_config.instance_count.max as usize)
            || (instances.len() < current_config.instance_count.min as usize)
        {
            slog::debug!(log, "Scaling up service"; 
            "service" => service_name.as_str(),
            "current_instances" => instances.len(),
            "instances_exceeding" => instances_exceeding);

            if let Err(e) = scale_up(&service_name, current_config.clone(), runtime.clone()).await {
                slog::error!(log, "Failed to scale up service"; 
                    "service" => service_name.as_str(), 
                    "error" => e.to_string());
            } else {
                run_proxy_for_service(service_name.to_string(), current_config.clone()).await;
            }
        }
        // Scale down if no thresholds defined or no instances exceeding defined thresholds
        else if (current_config.resource_thresholds.is_none() || instances_exceeding == 0)
            && instances.len() > current_config.instance_count.min as usize
        {
            // Find the instance with lowest resource usage
            if let Some((uuid, _, stats)) =
                instance_stats
                    .iter()
                    .min_by(|(_, _, stats_a), (_, _, stats_b)| {
                        stats_a
                            .cpu_percentage
                            .partial_cmp(&stats_b.cpu_percentage)
                            .unwrap()
                    })
            {
                slog::trace!(log, "Scaling down service"; 
                    "service" => service_name.as_str(),
                    "current_instances" => instances.len(),
                    "container_cpu" => stats.cpu_percentage);

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
                        "error" => e.to_string());
                } else {
                    run_proxy_for_service(service_name.to_string(), current_config.clone()).await;
                }
            }
        }

        // Wait for the next interval
        tokio::time::sleep(Duration::from_secs(
            current_config.interval_seconds.unwrap_or(30),
        ))
        .await;
    }
}

async fn scale_up(
    service_name: &str,
    config: ServiceConfig,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<()> {
    let instance_store = INSTANCE_STORE.get().unwrap();
    let mut instances = instance_store.entry(service_name.to_string()).or_default();

    let uuid = Uuid::new_v4();
    let container_name = format!("{}__{}", service_name, uuid);
    let port_range = config.exposed_port..config.exposed_port + 10;

    let exposed_port = runtime
        .start_container(
            &container_name,
            &config.image,
            config.target_port,
            port_range,
            config.memory_limit,
            config.cpu_limit,
        )
        .await?;

    instances.insert(
        uuid,
        InstanceMetadata {
            uuid,
            exposed_port,
            status: "running".to_string(),
        },
    );

    Ok(())
}

async fn scale_down(
    service_name: &str,
    target_uuid: Uuid,
    config: ServiceConfig,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<()> {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();

    if let Some(mut entry) = instance_store.get_mut(service_name) {
        if entry.len() > config.instance_count.min as usize {
            if let Some(metadata) = entry.get(&target_uuid) {
                let container_name = format!("{}__{}", service_name, target_uuid);
                let exposed_port = metadata.exposed_port;

                // Remove from upstreams
                let server_backends = SERVER_BACKENDS.get().unwrap();
                if let Some(backends) = server_backends.get(service_name) {
                    let addr = format!("127.0.0.1:{}", exposed_port);
                    backends.remove(&Backend::new(&addr).unwrap());
                    slog::trace!(log, "Removed backend from upstreams";
                        "service" => service_name,
                        "container" => container_name.clone(),
                        "address" => addr
                    );
                }

                // Graceful wait
                tokio::time::sleep(Duration::from_secs(10)).await;

                // Stop container
                if runtime.stop_container(&container_name).await.is_ok() {
                    entry.remove(&target_uuid);
                    slog::trace!(log, "Scaled down container";
                        "service" => service_name,
                        "container" => container_name);
                }
            }
        }
    }

    Ok(())
}
