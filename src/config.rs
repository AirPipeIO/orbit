// config.rs
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::OnceLock, time::Duration};
use tokio::sync::mpsc;
use uuid::Uuid;
use validator::Validate;

use crate::{
    container::{
        self, clean_up, manage, remove_reserved_ports, update_reserved_ports, InstanceMetadata,
        INSTANCE_STORE, RESERVED_PORTS, RUNTIME, SCALING_TASKS,
    },
    proxy,
    scale::auto_scale,
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
pub struct ResourceThresholds {
    pub cpu_percentage: Option<u8>,
    pub cpu_percentage_relative: Option<u8>,
    pub memory_percentage: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstanceCount {
    pub min: u8, // Minimum instances to keep running
    pub max: u8,
}

#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
pub struct ServiceConfig {
    #[validate(length(max = 210))]
    pub name: String,
    pub image: String,
    pub memory_limit: Option<Value>, // Examples: "512Mi", "1Gi"
    pub cpu_limit: Option<Value>,    // Examples: "0.5", "1", "2"
    pub target_port: u16,
    pub exposed_port: u16,
    // pub port_range: PortRange,
    pub resource_thresholds: Option<ResourceThresholds>,
    pub instance_count: InstanceCount,
    pub adopt_orphans: bool,
    pub interval_seconds: Option<u64>,
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
    let log: slog::Logger = slog_scope::logger();
    let config_store = CONFIG_STORE.get().unwrap();

    for path in event.paths.iter() {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                if path.exists() && path.is_file() {
                    match read_yaml_config(path).await {
                        Ok(config) => {
                            let service_name = config.name.clone();
                            update_reserved_ports(&service_name, config.exposed_port);

                            // Store both path and config
                            config_store.insert(
                                path.display().to_string(),
                                (path.to_path_buf(), config.clone()),
                            );

                            if let Err(e) = handle_config_update(&service_name, config).await {
                                slog::error!(log, "Failed to handle config update";
                                    "service" => service_name,
                                    "error" => e.to_string()
                                );
                            }
                        }
                        Err(e) => {
                            slog::error!(log, "config issue";
                                "file" => path.to_str(),
                                "error" => e.to_string()
                            );
                        }
                    }
                }
            }
            EventKind::Remove(_) => {
                if let Some((_, (_, config))) = config_store.remove(&path.display().to_string()) {
                    remove_reserved_ports(&config.name);
                    clean_up(&config.name).await;
                }
            }
            _ => {}
        }
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
    let reserved_ports = RESERVED_PORTS.get_or_init(DashMap::new);
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
                    reserved_ports.insert(config.exposed_port, config.name.clone());

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

    // Find containers matching the naming convention
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
        // Bulk adopt containers
        let mut instances = instance_store.entry(service_name.to_string()).or_default();
        let mut adopted_count = 0;

        for container in orphaned_containers {
            if container.port > 0 {
                if let Ok(uuid) = extract_uuid_from_name(&container.name) {
                    instances.insert(
                        uuid,
                        InstanceMetadata {
                            uuid,
                            exposed_port: container.port,
                            status: "adopted".to_string(),
                        },
                    );
                    adopted_count += 1;
                }
            }
        }

        slog::info!(log, "Adopted orphaned containers";
            "service" => service_name,
            "adopted" => adopted_count.to_string()
        );
    } else {
        // Bulk remove containers
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

        // Wait for all removals to complete with timeout
        let _ =
            tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(futures)).await;

        slog::info!(log, "Removed orphaned containers";
            "service" => %service_name,
            "count" => orphan_count
        );
    }

    Ok(())
}

fn extract_uuid_from_name(container_name: &str) -> Result<Uuid> {
    if let Some(uuid_str) = container_name.split("__").nth(1) {
        Uuid::parse_str(uuid_str).map_err(|e| anyhow!("Invalid UUID in container name: {}", e))
    } else {
        Err(anyhow!(
            "Container name does not contain UUID: {}",
            container_name
        ))
    }
}

pub async fn stop_service(service_name: &str) {
    let log = slog_scope::logger();
    let scaling_tasks = SCALING_TASKS.get().unwrap();
    let instance_store = INSTANCE_STORE.get().unwrap();

    // Stop the scaling task
    if let Some((_, handle)) = scaling_tasks.remove(service_name) {
        handle.abort(); // Cancel the task
        slog::trace!(log, "Scaling task aborted"; "service" => service_name);
    }

    // Clean up instance store
    if let Some((_, instances)) = instance_store.remove(service_name) {
        for (uuid, _metadata) in instances {
            let container_name = format!("{}__{}", service_name, uuid);
            let runtime = RUNTIME.get().unwrap().clone();

            if runtime.stop_container(&container_name).await.is_ok() {
                slog::trace!(log, "Stopped container"; "service" => service_name, "container" => container_name);
            }
        }
    }

    slog::info!(log, "Service stopped"; "service" => service_name);
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

pub async fn handle_config_update(service_name: &str, config: ServiceConfig) -> Result<()> {
    let log = slog_scope::logger();

    slog::debug!(log, "Starting config update process";
        "service" => service_name,
        "thresholds" => format!("{:?}", config.resource_thresholds)); // Add logging for thresholds

    // Send pause signal
    if let Some(sender) = CONFIG_UPDATES.get() {
        sender
            .send((service_name.to_string(), ScaleMessage::ConfigUpdate))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send config update: {}", e))?;
    }

    // Add a small delay to ensure scaling tasks receive the pause signal
    tokio::time::sleep(Duration::from_millis(100)).await;

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

    // Send resume signal
    if let Some(sender) = CONFIG_UPDATES.get() {
        sender
            .send((service_name.to_string(), ScaleMessage::Resume))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send resume signal: {}", e))?;
    }

    slog::debug!(log, "Completed config update process";
        "service" => service_name);

    Ok(())
}
