// config.rs
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::OnceLock, time::Duration};
use tokio::sync::mpsc;
use uuid::Uuid;
use validator::Validate;

use crate::{
    container::{
        self, clean_up, find_host_port, manage, remove_container_stats, remove_reserved_ports, update_reserved_ports, ContainerInfo, ContainerMetadata, ContainerPortMetadata, ContainerStats, InstanceMetadata, INSTANCE_STORE, RUNTIME, SCALING_TASKS
    },
    proxy::{self, SERVER_BACKENDS},
    scale::auto_scale,
    status::update_instance_store_cache,
};

#[derive(Clone)]
pub enum ScaleMessage {
    ConfigUpdate, // Added version number
    Resume,       // Resume with version to ensure matching
}
// Change CONFIG_UPDATES to use ScaleMessage
pub static CONFIG_UPDATES: OnceLock<mpsc::Sender<(String, ScaleMessage)>> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstanceCount {
    pub min: u8, // Minimum instances to keep running
    pub max: u8,
}

use crate::container::Container;

#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
pub struct ServiceConfig {
    #[validate(length(max = 210))]
    pub name: String,
    pub spec: ServiceSpec,
    pub memory_limit: Option<Value>,
    pub cpu_limit: Option<Value>,
    pub resource_thresholds: Option<ResourceThresholds>,
    pub instance_count: InstanceCount,
    pub adopt_orphans: bool,
    pub interval_seconds: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServiceSpec {
    pub containers: Vec<Container>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum PodMetricsStrategy {
    #[serde(rename = "max")]
    Maximum,
    #[serde(rename = "average")]
    Average,
}

impl Default for PodMetricsStrategy {
    fn default() -> Self {
        Self::Maximum
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResourceThresholds {
    pub cpu_percentage: Option<u8>,
    pub cpu_percentage_relative: Option<u8>,
    pub memory_percentage: Option<u8>,
    #[serde(default)]
    pub metrics_strategy: PodMetricsStrategy,
}

#[derive(Debug)]
pub struct PodStats {
    pub cpu_percentage: f64,
    pub cpu_percentage_relative: f64,
    pub memory_usage: u64,
    pub memory_limit: u64,
}

pub fn aggregate_pod_stats(
    container_stats: &[(Uuid, InstanceMetadata, ContainerStats)],
    strategy: &PodMetricsStrategy,
) -> PodStats {
    match strategy {
        PodMetricsStrategy::Maximum => {
            let mut max_stats = PodStats {
                cpu_percentage: 0.0,
                cpu_percentage_relative: 0.0,
                memory_usage: 0,
                memory_limit: 0,
            };

            for stats in container_stats {
                max_stats.cpu_percentage = max_stats.cpu_percentage.max(stats.2.cpu_percentage);
                max_stats.cpu_percentage_relative = max_stats
                    .cpu_percentage_relative
                    .max(stats.2.cpu_percentage_relative);
                max_stats.memory_usage = max_stats.memory_usage.max(stats.2.memory_usage);
                max_stats.memory_limit = max_stats.memory_limit.max(stats.2.memory_limit);
            }

            max_stats
        }
        PodMetricsStrategy::Average => {
            let count = container_stats.len() as f64;
            let sum_stats = container_stats.iter().fold(
                PodStats {
                    cpu_percentage: 0.0,
                    cpu_percentage_relative: 0.0,
                    memory_usage: 0,
                    memory_limit: 0,
                },
                |mut acc, stats| {
                    acc.cpu_percentage += stats.2.cpu_percentage;
                    acc.cpu_percentage_relative += stats.2.cpu_percentage_relative;
                    acc.memory_usage += stats.2.memory_usage;
                    acc.memory_limit += stats.2.memory_limit;
                    acc
                },
            );

            PodStats {
                cpu_percentage: sum_stats.cpu_percentage / count,
                cpu_percentage_relative: sum_stats.cpu_percentage_relative / count,
                memory_usage: sum_stats.memory_usage / count as u64,
                memory_limit: sum_stats.memory_limit / count as u64,
            }
        }
    }
}

pub static CONFIG_STORE: OnceLock<DashMap<String, (PathBuf, ServiceConfig)>> = OnceLock::new();

// Helper functions to access configs
pub fn get_config_by_path(path: &str) -> Option<ServiceConfig> {
    CONFIG_STORE
        .get()
        .and_then(|store| store.get(path).map(|entry| entry.value().1.clone()))
}

pub fn get_config_by_service(service_name: &str) -> Option<ServiceConfig> {
    CONFIG_STORE.get().and_then(|store| {
        store.iter().find_map(|entry| {
            if entry.value().1.name == service_name {
                Some(entry.value().1.clone())
            } else {
                None
            }
        })
    })
}

pub async fn watch_directory(config_dir: PathBuf) -> notify::Result<()> {
    let log: slog::Logger = slog_scope::logger();

    let (tx, mut rx) = mpsc::channel(100);

    let tx_clone: mpsc::Sender<DebouncedEvent> = tx.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(100), // Adjust as needed
        None,
        move |res: DebounceEventResult| {
            let tx = tx_clone.clone();
            if let Ok(events) = res {
                for event in events {
                    if event.paths.iter().any(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .map_or(false, |ext| ext == "yml" || ext == "yaml")
                    }) && matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        let _ = tx.blocking_send(event);
                    }
                }
            }
        },
    )?;

    debouncer.watch(&config_dir, RecursiveMode::Recursive)?;
    slog::debug!(log, "watching directory"; "directory" => config_dir.to_str());

    while let Some(event) = rx.recv().await {
        process_event(event).await;
    }

    Ok(())
}

async fn process_event(event: DebouncedEvent) {
    let config_store = CONFIG_STORE.get().unwrap();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    // Process the immediate event
    for path in event.paths.iter() {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                if path.exists() && path.is_file() {
                    // Check if it's a YAML file
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if ext != "yml" && ext != "yaml" {
                            slog::debug!(slog_scope::logger(), "Ignoring non-YAML file";
                                "path" => path.to_str(),
                                "extension" => ext
                            );
                            continue;
                        }
                    }

                    match read_yaml_config(path).await {
                        Ok(config) => {
                            let service_name = config.name.clone();

                            slog::info!(slog_scope::logger(), "Processing YAML config";
                                "service" => &service_name,
                                "path" => path.to_str()
                            );

                            // Handle like initialization
                            update_reserved_ports(&config.name, &config.spec);

                            // Store config
                            config_store.insert(
                                path.display().to_string(),
                                (path.to_path_buf(), config.clone()),
                            );

                            // Stop existing scaling task if it exists
                            if let Some((_, handle)) = scaling_tasks.remove(&service_name) {
                                handle.abort();
                                slog::debug!(slog_scope::logger(), "Aborted existing scaling task";
                                    "service" => &service_name
                                );
                            }

                            // Start containers and proxy
                            container::manage(&service_name, config.clone()).await;
                            proxy::run_proxy_for_service(service_name.clone(), config.clone())
                                .await;

                            let svc_name = service_name.clone();

                            // Create new scaling task
                            let handle = tokio::spawn(async move {
                                auto_scale(svc_name).await;
                            });
                            scaling_tasks.insert(service_name.clone(), handle);

                            slog::info!(slog_scope::logger(), "Service initialization complete";
                                "service" => &service_name
                            );
                        }
                        Err(e) => {
                            slog::error!(slog_scope::logger(), "Failed to parse YAML config";
                                "file" => path.to_str(),
                                "error" => e.to_string()
                            );
                        }
                    }
                }
            }
            EventKind::Remove(_) => {
                // Handle explicit removal events
                if let Some((_, (_, config))) = config_store.remove(&path.display().to_string()) {
                    let service_name = config.name.clone();
                    slog::info!(slog_scope::logger(), "Config file removed, cleaning up service";
                        "service" => &service_name,
                        "path" => path.to_str()
                    );

                    remove_reserved_ports(&service_name);

                    // Stop scaling task
                    if let Some((_, handle)) = scaling_tasks.remove(&service_name) {
                        handle.abort();
                    }

                    tokio::spawn(async move {
                        stop_service(&service_name).await;
                        clean_up(&service_name).await;

                        slog::info!(slog_scope::logger(), "Service cleanup completed";
                            "service" => &service_name
                        );
                    });
                }
            }
            _ => {}
        }
    }

    // After processing the event, verify all tracked configs still exist
    let services_to_cleanup: Vec<_> = config_store
        .iter()
        .filter_map(|entry| {
            let path = &entry.value().0;
            let config = &entry.value().1;

            if !path.exists()
                || !matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yml") | Some("yaml")
                )
            {
                Some((path.clone(), config.name.clone()))
            } else {
                None
            }
        })
        .collect();

    for (path, service_name) in services_to_cleanup {
        slog::info!(slog_scope::logger(), "Config file no longer valid, cleaning up service";
            "service" => &service_name,
            "path" => path.to_str()
        );

        config_store.remove(&path.display().to_string());
        remove_reserved_ports(&service_name);

        // Stop scaling task
        if let Some((_, handle)) = scaling_tasks.remove(&service_name) {
            handle.abort();
        }

        let service_name_clone = service_name.clone();
        tokio::spawn(async move {
            stop_service(&service_name_clone).await;
            clean_up(&service_name_clone).await;

            slog::info!(slog_scope::logger(), "Service cleanup completed";
                "service" => &service_name_clone
            );
        });
    }
}

pub async fn read_yaml_config(path: &PathBuf) -> Result<ServiceConfig> {
    let log = slog_scope::logger();

    let path_str = path.to_str().unwrap();
    if path_str.ends_with(".yml") || path_str.ends_with(".yaml") {
        let contents = tokio::fs::read_to_string(path).await?;
        let config: ServiceConfig = serde_yaml::from_str(&contents)?;

        // Debug log the parsed thresholds
        if let Some(thresholds) = &config.resource_thresholds {
            slog::debug!(log, "Parsed config thresholds";
                    "service" => &config.name,
                    "cpu_percentage" => thresholds.cpu_percentage,
                    "cpu_relative" => thresholds.cpu_percentage_relative,
                    "memory_percentage" => thresholds.memory_percentage);
        }

        return Ok(config);
    }

    Err(anyhow!("Not a yaml file {:?}", path))
}

use std::fs;

pub async fn initialize_configs(config_dir: &PathBuf) -> Result<()> {
    let config_store = CONFIG_STORE.get().unwrap();
    let log = slog_scope::logger();

    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml")
            || path.extension().and_then(|ext| ext.to_str()) == Some("yml")
        {
            match read_yaml_config(&path).await {
                Ok(config) => {
                    slog::info!(log, "Initialising config";
                        "service" => &config.name,
                        "path" => path.display().to_string()
                    );

                    // Reserve the port
                    update_reserved_ports(&config.name, &config.spec);

                    config_store.insert(path.display().to_string(), (path.clone(), config.clone()));
                    // Handle orphaned containers based on the adopt_orphans flag
                    handle_orphans(&config).await?;

                    container::manage(&config.name, config.clone()).await;
                    proxy::run_proxy_for_service(config.name.to_string(), config.clone()).await;

                    // Start auto-scaling task
                    let service_name = config.name.clone();

                    let handle = tokio::spawn(async move {
                        auto_scale(service_name.clone()).await;
                    });

                    // Store the task handle
                    SCALING_TASKS
                        .get()
                        .unwrap()
                        .insert(config.name.clone(), handle);
                }
                Err(e) => {
                    slog::error!(log, "Failed to load config";
                        "file" => path.to_str(),
                        "error" => e.to_string()
                    );
                }
            }
        }
    }

    Ok(())
}

pub async fn handle_orphans(config: &ServiceConfig) -> Result<()> {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();
    let service_name = &config.name;

    let orphaned_containers = runtime
        .list_containers(Some(service_name))
        .await
        .map_err(|e| anyhow!("Failed to list containers: {:?}", e))?;

    if orphaned_containers.is_empty() {
        return Ok(());
    }

    let orphan_count = orphaned_containers.len();
    slog::info!(log, "Found orphaned containers";
        "service" => service_name,
        "count" => orphan_count
    );

    if config.adopt_orphans {
        let mut pod_containers: HashMap<Uuid, Vec<ContainerInfo>> = HashMap::new();

        for container in orphaned_containers {
            if let Ok(parts) = parse_container_name(&container.name) {
                pod_containers
                    .entry(parts.uuid)
                    .or_default()
                    .push(container);
            }
        }

       // Check if pods have all required containers
       let required_container_count = config.spec.containers.len();
       let incomplete_pod_uuids: Vec<_> = pod_containers
           .iter()
           .filter(|(_, containers)| containers.len() != required_container_count)
           .map(|(uuid, _)| *uuid)
           .collect();

       // Remove incomplete pods
       for uuid in &incomplete_pod_uuids {
           if let Some(containers) = pod_containers.get(uuid) {
               slog::info!(log, "Removing incomplete pod";
                   "service" => service_name,
                   "uuid" => %uuid,
                   "expected_containers" => required_container_count,
                   "actual_containers" => containers.len()
               );

               for container in containers {
                   if let Err(e) = runtime.stop_container(&container.name).await {
                       slog::error!(log, "Failed to remove container from incomplete pod";
                           "service" => service_name,
                           "container" => &container.name,
                           "error" => e.to_string()
                       );
                   }
               }
           }
       }

       // Remove the incomplete pods from our map
       for uuid in incomplete_pod_uuids {
           pod_containers.remove(&uuid);
       }

       // Process remaining complete pods
       let mut instances = instance_store.entry(service_name.to_string()).or_default();
       let mut adopted_count = 0;

        for (uuid, containers) in &pod_containers {
            let mut pod_metadata = Vec::new();

            for container in containers {
                // Parse container name to get service information
                match parse_container_name(&container.name) {
                    Ok(parts) => {
                        // Look up service config
                        if let Some(config) = get_config_by_service(&parts.service_name) {
                            // Find the container config that matches this container name
                            if let Some(container_config) = config
                                .spec
                                .containers
                                .iter()
                                .find(|c| c.name == parts.container_name)
                            {
                                // If we find port configuration, inspect container to get actual port mappings
                                if let Some(port_configs) = &container_config.ports {
                                    // Inspect the container to get actual port mappings
                                    match runtime.inspect_container(&container.name).await {
                                        Ok(stats) => {
                                            // Map the configured ports to actual host ports
                                            let port_metadata: Vec<ContainerPortMetadata> = port_configs
                                                .iter()
                                                .filter_map(|p| {
                                                    // Container port we're looking for
                                                    let container_port = p.target_port.unwrap_or(p.port);
                                                    
                                                    // Find the corresponding host port from container inspection
                                                    if let Some(host_port) = find_host_port(&stats, container_port) {
                                                        Some(ContainerPortMetadata {
                                                            port: host_port, // Use the actual host port
                                                            node_port: p.node_port.unwrap_or(p.port),
                                                        })
                                                    } else {
                                                        slog::warn!(log, "Could not find host port mapping";
                                                            "service" => service_name,
                                                            "container" => &container.name,
                                                            "container_port" => container_port
                                                        );
                                                        None
                                                    }
                                                })
                                                .collect();

                                            if !port_metadata.is_empty() {
                                                pod_metadata.push(ContainerMetadata {
                                                    name: container.name.clone(),
                                                    ports: port_metadata,
                                                    status: "adopted".to_string(),
                                                });
                                                adopted_count += 1;
                                            }
                                        }
                                        Err(e) => {
                                            slog::error!(log, "Failed to inspect container";
                                                "service" => service_name,
                                                "container" => &container.name,
                                                "error" => e.to_string()
                                            );
                                        }
                                    }
                                }
                            }
                        } else {
                            slog::warn!(log, "Could not find service config for orphaned container";
                                "service" => parts.service_name,
                                "container" => &container.name
                            );
                        }
                    }
                    Err(e) => {
                        slog::warn!(log, "Failed to parse container name";
                            "container" => &container.name,
                            "error" => e.to_string()
                        );
                    }
                }
            }

            if !pod_metadata.is_empty() {
                instances.insert(
                    *uuid,
                    InstanceMetadata {
                        uuid: *uuid,
                        containers: pod_metadata,
                    },
                );
            }
        }

        slog::info!(log, "Adopted orphaned containers";
            "service" => service_name,
            "adopted_pods" => pod_containers.len().to_string(),
            "adopted_containers" => adopted_count.to_string()
        );
    } else {
        let mut futures = Vec::new();
        let containers_to_remove = orphaned_containers.clone();

        for container in containers_to_remove {
            let runtime = runtime.clone();
            let container_name = container.name.clone();
            let service_name = service_name.to_string();

            futures.push(tokio::spawn(async move {
                if let Err(e) = runtime.stop_container(&container_name).await {
                    slog::error!(slog_scope::logger(), "Failed to remove orphaned container";
                        "service" => %service_name,
                        "container" => %container_name,
                        "error" => %e
                    );
                }
            }));
        }

        let _ = tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(futures)).await;

        slog::info!(log, "Removed orphaned containers";
            "service" => %service_name,
            "count" => orphan_count
        );
    }

    Ok(())
}


#[derive(Debug)]
pub struct ContainerNameParts {
    pub service_name: String,
    pub pod_number: u8,
    pub container_name: String,
    pub uuid: Uuid,
}

pub fn parse_container_name(container_name: &str) -> Result<ContainerNameParts> {
    let parts: Vec<&str> = container_name.split("__").collect();

    if parts.len() != 4 {
        return Err(anyhow!(
            "Container name does not match pattern 'service__pod-number__container-name__uuid': {}",
            container_name
        ));
    }

    let pod_number = parts[1].parse::<u8>().map_err(|e| {
        anyhow!(
            "Invalid pod number in container name '{}': {}",
            container_name,
            e
        )
    })?;

    let uuid = Uuid::parse_str(parts[3])
        .map_err(|e| anyhow!("Invalid UUID in container name '{}': {}", container_name, e))?;

    Ok(ContainerNameParts {
        service_name: parts[0].to_string(),
        pod_number,
        container_name: parts[2].to_string(),
        uuid,
    })
}

// Update the stop_service function to ensure complete cleanup
pub async fn stop_service(service_name: &str) {
    let log = slog_scope::logger();
    let scaling_tasks = SCALING_TASKS.get().unwrap();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let server_backends = SERVER_BACKENDS.get().unwrap();

    // Stop the scaling task
    if let Some((_, handle)) = scaling_tasks.remove(service_name) {
        handle.abort(); // Cancel the task
        slog::debug!(log, "Scaling task aborted"; "service" => service_name);
    }

    // Remove from load balancer
    server_backends.remove(service_name);

    // Clean up instance store and stop containers
    if let Some((_, instances)) = instance_store.remove(service_name) {
        for (uuid, _metadata) in instances {
            let container_name = format!("{}__{}", service_name, uuid);
            let runtime = RUNTIME.get().unwrap().clone();

            // Remove container stats
            remove_container_stats(service_name, &container_name);

            // Stop the container
            if let Err(e) = runtime.stop_container(&container_name).await {
                slog::error!(log, "Failed to stop container during service cleanup";
                    "service" => service_name,
                    "container" => &container_name,
                    "error" => e.to_string()
                );
            } else {
                slog::debug!(log, "Container stopped successfully";
                    "service" => service_name,
                    "container" => &container_name
                );
            }
        }
    }

    // Update instance store cache
    update_instance_store_cache();

    slog::info!(log, "Service stopped and cleaned up"; "service" => service_name);
}

pub fn parse_memory_limit(memory_limit: &serde_json::Value) -> Result<u64> {
    match memory_limit {
        serde_json::Value::Number(num) => {
            let value = num
                .as_f64()
                .ok_or_else(|| anyhow!("Invalid memory number"))?;
            Ok((value * 1024.0 * 1024.0 * 1024.0) as u64) // Assume value is in GiB
        }
        serde_json::Value::String(s) => {
            let re = regex::Regex::new(r"^(\d+\.?\d*)([KMG]i?)$")?;
            if let Some(caps) = re.captures(s) {
                let value: f64 = caps[1].parse()?;
                let multiplier = match &caps[2] {
                    "Ki" | "K" => 1024.0,
                    "Mi" | "M" => 1024.0 * 1024.0,
                    "Gi" | "G" => 1024.0 * 1024.0 * 1024.0,
                    _ => return Err(anyhow!("Unsupported memory unit: {}", &caps[2])),
                };
                return Ok((value * multiplier) as u64);
            }
            Err(anyhow!("Invalid memory limit format: {}", s))
        }
        _ => Err(anyhow!("Unsupported memory limit type")),
    }
}

pub fn parse_cpu_limit(cpu_limit: &serde_json::Value) -> Result<u64> {
    match cpu_limit {
        serde_json::Value::Number(num) => {
            let value = num.as_f64().ok_or_else(|| anyhow!("Invalid CPU number"))?;
            if value <= 0.0 || value > ((u64::MAX as f64) / 1_000_000_000.0) {
                return Err(anyhow!("CPU limit out of valid range"));
            }
            Ok((value * 1_000_000_000.0) as u64)
        }
        serde_json::Value::String(s) => {
            let value: f64 = s.parse()?;
            Ok((value * 1_000_000_000.0) as u64)
        }
        _ => Err(anyhow!("Unsupported CPU limit type")),
    }
}

// In config.rs, modify handle_config_update

pub async fn handle_config_update(service_name: &str, config: ServiceConfig) -> Result<()> {
    let log = slog_scope::logger();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    slog::debug!(log, "Starting config update process";
        "service" => service_name,
        "thresholds" => format!("{:?}", config.resource_thresholds));

    // Check if this is a new service (no existing scaling task)
    let is_new_service = !scaling_tasks.contains_key(service_name);

    if is_new_service {
        slog::info!(log, "Detected new service, initializing scaling task";
            "service" => service_name
        );

        // Initialize like a new service
        let service_name_clone = service_name.to_string();
        let handle = tokio::spawn(async move {
            auto_scale(service_name_clone).await;
        });
        scaling_tasks.insert(service_name.to_string(), handle);
    } else {
        // Existing service - send pause signal
        if let Some(sender) = CONFIG_UPDATES.get() {
            sender
                .send((service_name.to_string(), ScaleMessage::ConfigUpdate))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send config update: {}", e))?;
        }
    }

    // Update config in store
    if let Some(config_store) = CONFIG_STORE.get() {
        for mut entry in config_store.iter_mut() {
            if entry.value().1.name == service_name {
                entry.value_mut().1 = config.clone();
                break;
            }
        }
    }

    // Handle containers and proxy
    manage(service_name, config.clone()).await;
    proxy::run_proxy_for_service(service_name.to_string(), config.clone()).await;

    // If it's an existing service, send resume signal
    if !is_new_service {
        if let Some(sender) = CONFIG_UPDATES.get() {
            sender
                .send((service_name.to_string(), ScaleMessage::Resume))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send resume signal: {}", e))?;
        }
    }

    slog::debug!(log, "Completed config update process";
        "service" => service_name);

    Ok(())
}
