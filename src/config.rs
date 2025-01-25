// config.rs
use crate::{
    container::{Container, IMAGE_CHECK_TASKS},
    rolling_update,
};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, SystemTime},
};
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;
use validator::Validate;

use crate::{
    container::{
        self, clean_up, manage, remove_container_stats, ContainerInfo, ContainerMetadata,
        ContainerPortMetadata, ContainerStats, InstanceMetadata, INSTANCE_STORE, RUNTIME,
        SCALING_TASKS,
    },
    proxy::{self, SERVER_BACKENDS},
    scale::auto_scale,
    status::update_instance_store_cache,
};

use std::collections::HashSet;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RollingUpdateConfig {
    #[serde(default = "default_max_unavailable")]
    pub max_unavailable: u8,
    #[serde(default = "default_max_surge")]
    pub max_surge: u8,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

fn default_max_unavailable() -> u8 {
    1
}
fn default_max_surge() -> u8 {
    1
}

impl Default for RollingUpdateConfig {
    fn default() -> Self {
        Self {
            max_unavailable: default_max_unavailable(),
            max_surge: default_max_surge(),
            timeout: Duration::from_secs(300), // 5 minute default timeout
        }
    }
}

// Add new validation error types
#[derive(Error, Debug)]
pub enum ConfigValidationError {
    #[error("Duplicate service name '{0}' found")]
    DuplicateServiceName(String),
    #[error("Duplicate container name '{0}' found in service '{1}'")]
    DuplicateContainerName(String, String),
    #[error("Invalid service name '{0}': {1}")]
    InvalidServiceName(String, String),
    #[error("Invalid container name '{0}': {1}")]
    InvalidContainerName(String, String),
}

#[derive(Error, Debug)]
pub enum PortValidationError {
    #[error("Duplicate {port_type} port {port} found in service '{service}'")]
    DuplicatePortInService {
        port_type: String,
        port: u16,
        service: String,
    },
    #[error("{port_type} port {port} in service '{service1}' conflicts with service '{service2}'")]
    PortConflictBetweenServices {
        port_type: String,
        port: u16,
        service1: String,
        service2: String,
    },
}

// Validate ports within a single service config
pub fn validate_service_ports(config: &ServiceConfig) -> Result<(), PortValidationError> {
    let mut target_ports = HashSet::new();
    let mut node_ports = HashSet::new();

    for container in &config.spec.containers {
        if let Some(ports) = &container.ports {
            for port_config in ports {
                // Check target_ports against both target and node ports
                if let Some(target_port) = port_config.target_port {
                    if !target_ports.insert(target_port) || node_ports.contains(&target_port) {
                        return Err(PortValidationError::DuplicatePortInService {
                            port_type: "target".to_string(),
                            port: target_port,
                            service: config.name.clone(),
                        });
                    }
                }

                // Check node_ports against both node and target ports
                if let Some(node_port) = port_config.node_port {
                    if !node_ports.insert(node_port) || target_ports.contains(&node_port) {
                        return Err(PortValidationError::DuplicatePortInService {
                            port_type: "node".to_string(),
                            port: node_port,
                            service: config.name.clone(),
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

// Check for port conflicts between services
pub fn check_port_conflicts(
    new_config: &ServiceConfig,
    exclude_service: Option<&str>,
) -> Result<(), PortValidationError> {
    let config_store = CONFIG_STORE.get().expect("Config store not initialized");

    // Collect ports from new config
    let mut new_target_ports = HashSet::new();
    let mut new_node_ports = HashSet::new();

    for container in &new_config.spec.containers {
        if let Some(ports) = &container.ports {
            for port_config in ports {
                if let Some(target_port) = port_config.target_port {
                    new_target_ports.insert(target_port);
                }
                if let Some(node_port) = port_config.node_port {
                    new_node_ports.insert(node_port);
                }
            }
        }
    }

    // Check against all existing services
    for entry in config_store.iter() {
        let existing_config = &entry.value().1;

        // Skip if this is the service we're updating
        if let Some(exclude) = exclude_service {
            if existing_config.name == exclude {
                continue;
            }
        }

        // Skip comparing against self
        if existing_config.name == new_config.name {
            continue;
        }

        for container in &existing_config.spec.containers {
            if let Some(ports) = &container.ports {
                for port_config in ports {
                    // Check for conflicts between target_ports and existing node_ports, and vice versa
                    if let Some(target_port) = port_config.target_port {
                        if new_target_ports.contains(&target_port)
                            || new_node_ports.contains(&target_port)
                        {
                            return Err(PortValidationError::PortConflictBetweenServices {
                                port_type: "target".to_string(),
                                port: target_port,
                                service1: new_config.name.clone(),
                                service2: existing_config.name.clone(),
                            });
                        }
                    }

                    if let Some(node_port) = port_config.node_port {
                        if new_node_ports.contains(&node_port)
                            || new_target_ports.contains(&node_port)
                        {
                            return Err(PortValidationError::PortConflictBetweenServices {
                                port_type: "node".to_string(),
                                port: node_port,
                                service1: new_config.name.clone(),
                                service2: existing_config.name.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// Add validation functions
fn validate_service_name(name: &str) -> Result<(), ConfigValidationError> {
    // RFC 1123 DNS label validation
    let name_regex = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,61}[a-z0-9]$").unwrap();
    if !name_regex.is_match(name) {
        return Err(ConfigValidationError::InvalidServiceName(
            name.to_string(),
            "Service name must be a valid DNS label: lowercase alphanumeric characters or '-', starting and ending with alphanumeric".to_string(),
        ));
    }
    Ok(())
}

fn validate_container_name(name: &str) -> Result<(), ConfigValidationError> {
    // Similar validation for container names
    let name_regex = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,61}[a-z0-9]$").unwrap();
    if !name_regex.is_match(name) {
        return Err(ConfigValidationError::InvalidContainerName(
            name.to_string(),
            "Container name must be a valid DNS label: lowercase alphanumeric characters or '-', starting and ending with alphanumeric".to_string(),
        ));
    }
    Ok(())
}

// Add this function to check for duplicate service names
pub fn check_service_name_uniqueness(
    config: &ServiceConfig,
    exclude_service: Option<&str>,
) -> Result<(), ConfigValidationError> {
    let config_store = CONFIG_STORE.get().expect("Config store not initialized");

    // Check if any existing config has the same name, excluding the service being updated
    for existing_config in config_store.iter() {
        if existing_config.value().1.name == config.name {
            // Skip if this is the service we're updating
            if let Some(exclude) = exclude_service {
                println!("\n\n{:?} : {:?} \n\n", exclude, config.name);
                if exclude == config.name {
                    continue;
                }
            }
            return Err(ConfigValidationError::DuplicateServiceName(
                config.name.clone(),
            ));
        }
    }

    Ok(())
}

// Add this function to check for duplicate container names within a service
fn check_container_name_uniqueness(config: &ServiceConfig) -> Result<(), ConfigValidationError> {
    let mut container_names = HashSet::new();

    for container in &config.spec.containers {
        if !container_names.insert(&container.name) {
            return Err(ConfigValidationError::DuplicateContainerName(
                container.name.clone(),
                config.name.clone(),
            ));
        }

        // Also validate the container name format
        validate_container_name(&container.name)?;
    }

    Ok(())
}

#[derive(Clone, Debug)]
pub enum ScaleMessage {
    ConfigUpdate, // Added version number
    Resume,       // Resume with version to ensure matching
    RollingUpdate,
    RollingUpdateComplete,
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
    #[serde(with = "humantime_serde", default)]
    pub image_check_interval: Option<Duration>,
    pub rolling_update_config: Option<RollingUpdateConfig>,
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
        process_event(event, &config_dir).await;
    }

    Ok(())
}

pub fn get_relative_config_path(full_path: &PathBuf, config_dir: &PathBuf) -> Option<String> {
    let config_dir_str = config_dir.to_str()?;
    let full_path_str = full_path.to_str()?;

    // Find the position of "configs/" in the full path
    if let Some(pos) = full_path_str.find(config_dir_str) {
        // Extract everything from "configs/" onwards
        let rel_path = &full_path_str[pos..];
        return Some(rel_path.to_string());
    }
    None
}

async fn process_event(event: DebouncedEvent, config_dir: &PathBuf) {
    let config_store = CONFIG_STORE.get().unwrap();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    // Process the immediate event
    for path in event.paths.iter() {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                if path.exists() && path.is_file() {
                    let rel_config_path = get_relative_config_path(path, config_dir).unwrap();
                    // Check if there's an existing config for this path

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

                    let existing_service = match event.kind {
                        EventKind::Modify(_) => {
                            if let Some(config) = get_config_by_path(&rel_config_path) {
                                Some(config.name)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    match read_yaml_config(path, existing_service.as_deref()).await {
                        Ok(config) => {
                            let service_name = config.name.clone();

                            slog::info!(slog_scope::logger(), "Processing YAML config";
                                "service" => &service_name,
                                "path" => path.to_str()
                            );

                            // Store config
                            config_store
                                .insert(rel_config_path, (path.to_path_buf(), config.clone()));

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

pub async fn read_yaml_config(
    path: &PathBuf,
    exclude_service: Option<&str>,
) -> Result<ServiceConfig> {
    let log = slog_scope::logger();

    let path_str = path.to_str().unwrap();
    if path_str.ends_with(".yml") || path_str.ends_with(".yaml") {
        let contents = tokio::fs::read_to_string(path).await?;
        let config: ServiceConfig = serde_yaml::from_str(&contents)?;

        // Validate service name format
        validate_service_name(&config.name)?;

        // Check for duplicate service names (no exclusion for new configs)
        check_service_name_uniqueness(&config, exclude_service)?;

        // Check for duplicate container names
        check_container_name_uniqueness(&config)?;

        // Validate ports within the service
        validate_service_ports(&config)?;

        // Check for conflicts with other services
        check_port_conflicts(&config, None)?;

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
            match read_yaml_config(&path, None).await {
                Ok(config) => {
                    slog::info!(log, "Initialising config";
                        "service" => &config.name,
                        "path" => path.display().to_string()
                    );

                    config_store.insert(path.display().to_string(), (path.clone(), config.clone()));
                    // Handle orphaned containers based on the adopt_orphans flag
                    handle_orphans(&config).await?;

                    container::manage(&config.name, config.clone()).await;
                    proxy::run_proxy_for_service(config.name.to_string(), config.clone()).await;

                    // Start auto-scaling task
                    let service_name: String = config.name.clone();

                    let handle = tokio::spawn(async move {
                        auto_scale(service_name.clone()).await;
                    });

                    // Store the task handle
                    SCALING_TASKS
                        .get()
                        .unwrap()
                        .insert(config.name.clone(), handle);

                    let service_name: String = config.name.clone();
                    let svc_name: String = config.name.clone();

                    let handle = tokio::spawn(async move {
                        if let Err(e) =
                            rolling_update::start_image_check_task(service_name.clone(), config)
                                .await
                        {
                            slog::error!(slog_scope::logger(), "Image check task failed";
                                "error" => e.to_string()
                            );
                        }
                    });
                    IMAGE_CHECK_TASKS
                        .get()
                        .unwrap()
                        .insert(svc_name.clone(), handle);
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

    let orphaned_containers = runtime.list_containers(Some(service_name)).await?;
    if orphaned_containers.is_empty() {
        return Ok(());
    }

    let orphan_count = orphaned_containers.len();
    slog::info!(log, "Found orphaned containers"; "service" => service_name, "count" => orphan_count);

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

        let required_container_count = config.spec.containers.len();
        let incomplete_pod_uuids: Vec<_> = pod_containers
            .iter()
            .filter(|(_, containers)| containers.len() != required_container_count)
            .map(|(uuid, _)| *uuid)
            .collect();

        for uuid in &incomplete_pod_uuids {
            if let Some(containers) = pod_containers.get(uuid) {
                let network_name = format!("{}__{}", service_name, uuid);

                for container in containers {
                    if let Err(e) = runtime.stop_container(&container.name).await {
                        slog::error!(log, "Failed to remove container from incomplete pod";
                            "service" => service_name,
                            "container" => &container.name,
                            "error" => e.to_string()
                        );
                    }
                }

                if let Err(e) = runtime.remove_pod_network(&network_name).await {
                    slog::error!(log, "Failed to remove network";
                        "service" => service_name,
                        "network" => &network_name,
                        "error" => e.to_string()
                    );
                }
            }
        }

        for uuid in incomplete_pod_uuids {
            pod_containers.remove(&uuid);
        }

        let mut instances = instance_store.entry(service_name.to_string()).or_default();
        let mut adopted_count = 0;

        for (uuid, containers) in &pod_containers {
            let network_name = format!("{}__{}", service_name, uuid);
            let mut pod_metadata = Vec::new();

            for container in containers {
                if let Ok(parts) = parse_container_name(&container.name) {
                    if let Some(container_config) = config
                        .spec
                        .containers
                        .iter()
                        .find(|c| c.name == parts.container_name)
                    {
                        if let Some(port_configs) = &container_config.ports {
                            if let Ok(container_data) =
                                runtime.inspect_container(&container.name).await
                            {
                                let port_metadata: Vec<ContainerPortMetadata> = port_configs
                                    .iter()
                                    .map(|p| ContainerPortMetadata {
                                        port: p.port,
                                        target_port: p.target_port,
                                        node_port: p.node_port,
                                    })
                                    .collect();

                                pod_metadata.push(ContainerMetadata {
                                    name: container.name.clone(),
                                    network: network_name.clone(),
                                    ip_address: container_data.ip_address,
                                    ports: port_metadata,
                                    status: "adopted".to_string(),
                                });
                                adopted_count += 1;
                            } else {
                                slog::error!(log, "Failed to inspect container";
                                    "service" => service_name,
                                    "container" => &container.name,
                                );
                            }
                        }
                    }
                }
            }

            if !pod_metadata.is_empty() {
                let now = SystemTime::now();
                let mut image_hashes = HashMap::new();

                // Try to get image hashes for existing containers
                for container in containers {
                    if let Ok(parts) = parse_container_name(&container.name) {
                        if let Some(container_config) = config
                            .spec
                            .containers
                            .iter()
                            .find(|c| c.name == parts.container_name)
                        {
                            if let Ok(hash) =
                                runtime.get_image_digest(&container_config.image).await
                            {
                                image_hashes.insert(container_config.name.clone(), hash);
                            }
                        }
                    }
                }

                instances.insert(
                    *uuid,
                    InstanceMetadata {
                        uuid: *uuid,
                        created_at: now,
                        network: network_name,
                        image_hash: image_hashes,
                        containers: pod_metadata,
                    },
                );
            }
        }

        slog::info!(log, "Adopted orphaned containers";
            "service" => service_name,
            "adopted_pods" => pod_containers.len(),
            "adopted_containers" => adopted_count.to_string()
        );
    } else {
        // Group containers by their network
        let mut network_containers: HashMap<String, Vec<String>> = HashMap::new();

        for container in &orphaned_containers {
            if let Ok(parts) = parse_container_name(&container.name) {
                let network_name = format!("{}_{}", service_name, parts.uuid);
                network_containers
                    .entry(network_name)
                    .or_default()
                    .push(container.name.clone());
            }
        }

        // Process each network and its containers
        for (network_name, containers) in network_containers {
            // First stop all containers in the network
            let mut stop_futures = Vec::new();
            for container_name in containers {
                let runtime = runtime.clone();
                let service_name = service_name.to_string();

                stop_futures.push(tokio::spawn(async move {
                    if let Err(e) = runtime.stop_container(&container_name).await {
                        slog::error!(slog_scope::logger(), "Failed to remove orphaned container";
                            "service" => %service_name,
                            "container" => %container_name,
                            "error" => %e
                        );
                        return Err(e);
                    }
                    Ok(())
                }));
            }

            // Wait for all containers to be stopped
            let _ = futures::future::join_all(stop_futures).await;

            // Add a small delay to ensure Docker has processed container removals
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Then try to remove the network
            if let Err(e) = runtime.remove_pod_network(&network_name).await {
                slog::error!(slog_scope::logger(), "Failed to remove orphaned network";
                    "service" => %service_name,
                    "network" => %network_name,
                    "error" => %e
                );
            }
        }
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
    // Stop both the scaling task and image checker
    for task_name in [service_name, &format!("{}_updater", service_name)] {
        if let Some((_, handle)) = scaling_tasks.remove(task_name) {
            handle.abort();
            slog::debug!(log, "Task aborted";
                "service" => service_name,
                "task" => task_name
            );
        }
    }

    // Stop the image check task
    if let Some((_, handle)) = IMAGE_CHECK_TASKS.get().unwrap().remove(service_name) {
        handle.abort();
        slog::debug!(log, "Image check task aborted"; "service" => service_name);
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
    let _ = update_instance_store_cache();

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

    // Validate service name format
    validate_service_name(&config.name)?;

    // Check for duplicate container names
    check_container_name_uniqueness(&config)?;

    // Validate ports within the service
    validate_service_ports(&config)?;

    // Check for conflicts with other services
    check_port_conflicts(&config, None)?;

    // Only check for service name uniqueness if it's different from the current name
    if service_name != config.name {
        check_service_name_uniqueness(&config, Some(service_name))?;
    }

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
