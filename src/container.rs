//container.rs
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, Stats,
    StatsOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::secret::{Mount, MountTypeEnum};
use bollard::Docker;
use dashmap::DashMap;
use futures::StreamExt;
use pingora_load_balancing::Backend;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{
    get_config_by_service, parse_container_name, parse_cpu_limit, parse_memory_limit,
    ServiceConfig, ServiceSpec,
};
use crate::proxy::SERVER_BACKENDS;
use crate::status::update_instance_store_cache;

const MAX_SERVICE_NAME_LENGTH: usize = 60; // Common k8s practice
const MAX_CONTAINER_NAME_LENGTH: usize = 60; // This gives us plenty of room

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VolumeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
}

// Update Container struct to include volume mounts
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Container {
    pub name: String,
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<ContainerPort>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_mounts: Option<Vec<VolumeMount>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<HashMap<String, VolumeData>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerPort {
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Protocol {
    TCP,
    UDP,
}

use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum ContainerError {
    #[error("Service name exceeds maximum length of {0} characters")]
    ServiceNameTooLong(usize),
    #[error("Container name exceeds maximum length of {0} characters")]
    ContainerNameTooLong(usize),
}

impl Container {
    pub fn generate_runtime_name(
        &self,
        service_name: &str,
        pod_number: u8,
        uuid: &str,
    ) -> Result<String, ContainerError> {
        if service_name.len() > MAX_SERVICE_NAME_LENGTH {
            return Err(ContainerError::ServiceNameTooLong(MAX_SERVICE_NAME_LENGTH).into());
        }
        if self.name.len() > MAX_CONTAINER_NAME_LENGTH {
            return Err(ContainerError::ContainerNameTooLong(MAX_CONTAINER_NAME_LENGTH).into());
        }

        // Format: service-name__pod-number__container-name__uuid
        Ok(format!(
            "{service_name}__{pod_number}__{}__{uuid}",
            self.name
        ))
    }
}

#[derive(Clone, Debug)]
pub struct ServiceStats {
    container_stats: DashMap<String, ContainerStats>,
}

impl ServiceStats {
    fn new() -> Self {
        Self {
            container_stats: DashMap::new(),
        }
    }

    pub fn update_stats(&self, container_name: &str, stats: ContainerStats) {
        self.container_stats
            .insert(container_name.to_string(), stats);
    }

    pub fn remove_container(&self, container_name: &str) {
        self.container_stats.remove(container_name);
    }

    pub fn get_container_stats(&self, container_name: &str) -> Option<ContainerStats> {
        self.container_stats.get(container_name).map(|s| s.clone())
    }
}

pub static SERVICE_STATS: OnceLock<DashMap<String, ServiceStats>> = OnceLock::new();

// Update the update_container_stats function to use service-level stats
pub async fn update_container_stats(
    service_name: &str,
    container_name: &str,
    stats: Stats,
    nano_cpus: Option<u64>,
) -> ContainerStats {
    let stats_store = CONTAINER_STATS.get().expect("Stats store not initialized");
    let service_stats = SERVICE_STATS.get().expect("Service stats not initialized");

    let now = SystemTime::now();
    let cpu_total = stats.cpu_stats.cpu_usage.total_usage;
    let system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
    let online_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;

    // Get previous stats with minimal lock time
    let previous = stats_store.get(container_name).map(|entry| StatsEntry {
        timestamp: entry.timestamp,
        cpu_total_usage: entry.cpu_total_usage,
        system_cpu_usage: entry.system_cpu_usage,
    });

    let (cpu_percentage, cpu_percentage_relative) = calculate_cpu_percentages(
        previous.as_ref(),
        cpu_total,
        system_cpu,
        online_cpus,
        nano_cpus,
    );

    // Update historical stats
    let stats_entry = StatsEntry {
        timestamp: now,
        cpu_total_usage: cpu_total,
        system_cpu_usage: system_cpu,
    };
    stats_store.insert(container_name.to_string(), stats_entry);

    let container_stats = ContainerStats {
        id: stats.id.clone(),
        cpu_percentage,
        cpu_percentage_relative,
        memory_usage: stats.memory_stats.usage.unwrap_or(0),
        memory_limit: stats.memory_stats.limit.unwrap_or(0),
        port_mappings: HashMap::new(), // Initialize empty, will be populated by inspect_container
        timestamp: now,
    };

    // Update service-level stats
    service_stats
        .entry(service_name.to_string())
        .or_insert_with(ServiceStats::new)
        .update_stats(container_name, container_stats.clone());

    container_stats
}

pub fn find_host_port(stats: &ContainerStats, container_port: u16) -> Option<u16> {
    stats.port_mappings.get(&container_port).copied()
}

// Update remove_container_stats to handle service-level cleanup
pub fn remove_container_stats(service_name: &str, container_name: &str) {
    if let Some(stats_store) = CONTAINER_STATS.get() {
        stats_store.remove(container_name);
    }

    if let Some(service_stats) = SERVICE_STATS.get() {
        if let Some(stats) = service_stats.get(service_name) {
            stats.remove_container(container_name);
        }
    }
}

// Initialize service stats in main.rs
pub fn initialize_stats() {
    SERVICE_STATS.get_or_init(DashMap::new);
    CONTAINER_STATS.get_or_init(DashMap::new);
}

pub static RUNTIME: OnceLock<Arc<dyn ContainerRuntime>> = OnceLock::new();

pub static RESERVED_PORTS: OnceLock<DashMap<u16, String>> = OnceLock::new();

pub static INSTANCE_STORE: OnceLock<DashMap<String, HashMap<Uuid, InstanceMetadata>>> =
    OnceLock::new();

pub static PORT_RANGE: OnceLock<Range<u16>> = OnceLock::new();

// Global registry for scaling tasks
pub static SCALING_TASKS: OnceLock<DashMap<String, JoinHandle<()>>> = OnceLock::new();

// Global stats history store
#[derive(Clone, Deserialize, Serialize)]
pub struct StatsEntry {
    pub timestamp: SystemTime,
    pub cpu_total_usage: u64,
    pub system_cpu_usage: u64,
}

// Global stats history store
pub static CONTAINER_STATS: OnceLock<DashMap<String, StatsEntry>> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstanceMetadata {
    pub uuid: Uuid,
    pub containers: Vec<ContainerMetadata>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerMetadata {
    pub name: String,
    pub ports: Vec<ContainerPortMetadata>,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerPortMetadata {
    pub port: u16,      // The container's port
    pub node_port: u16, // The external port
}

// Define the container runtime trait
#[async_trait]
pub trait ContainerRuntime: Send + Sync + std::fmt::Debug {
    // async fn start_container(
    //     &self,
    //     name: &str,
    //     image: &str,
    //     target_port: u16,
    //     memory_limit: Option<Value>, // Human-readable memory limit (e.g., "512Mi")
    //     cpu_limit: Option<Value>,    // Human-readable CPU limit (e.g., "0.5")
    // ) -> Result<u16>;
    async fn start_containers(
        &self,
        service_name: &str,
        pod_number: u8,
        containers: &Vec<Container>,
        memory_limit: Option<Value>,
        cpu_limit: Option<Value>,
    ) -> Result<Vec<(String, Vec<ContainerPortMetadata>)>>; // Returns vec of (container_name, ports)
    async fn stop_container(&self, name: &str) -> Result<()>;
    async fn inspect_container(&self, name: &str) -> Result<ContainerStats>;
    async fn list_containers(&self, service_name: Option<&str>) -> Result<Vec<ContainerInfo>>;
}

// Container information struct
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerInfo {
    pub id: String,    // Container ID
    pub name: String,  // Container name
    pub state: String, // Container state (e.g., "running")
    pub port: u16,     // Exposed port, if available
}

// Implementation for Docker
#[derive(Debug, Clone)]
pub struct DockerRuntime {
    client: Docker,
}

// Struct to store container performance stats
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerStats {
    pub id: String,
    pub cpu_percentage: f64,
    pub cpu_percentage_relative: f64,
    pub memory_usage: u64,
    pub memory_limit: u64,
    pub port_mappings: HashMap<u16, u16>, // container_port -> host_port
    timestamp: SystemTime,
}

pub fn update_reserved_ports(service_name: &str, spec: &ServiceSpec) {
    let reserved_ports = RESERVED_PORTS.get().unwrap();

    // For each container in the service
    for container in &spec.containers {
        if let Some(ports) = &container.ports {
            for port_config in ports {
                reserved_ports.insert(port_config.port, service_name.to_string());
            }
        }
    }
}

pub fn remove_reserved_ports(service_name: &str) {
    let reserved_ports = RESERVED_PORTS.get().unwrap();
    reserved_ports.retain(|_, v| v != service_name);
}

impl DockerRuntime {
    pub fn new() -> Result<Self> {
        let client = Docker::connect_with_local_defaults()
            .map_err(|e| anyhow!("Failed to connect to Docker: {:?}", e))?;
        Ok(Self { client })
    }

    fn is_port_available(port: u16) -> bool {
        use std::net::TcpListener;

        // Check if the port is available system-wide
        if !TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return false;
        }

        // Check against reserved ports
        let reserved_ports = RESERVED_PORTS.get().unwrap();
        if reserved_ports.contains_key(&port) {
            return false;
        }

        true
    }

    fn find_available_port() -> Option<u16> {
        let port_range = PORT_RANGE.get().expect("Port range not initialized");
        port_range
            .clone()
            .find(|port| Self::is_port_available(*port))
    }

    async fn setup_volume_mounts(
        &self,
        container: &Container,
        container_name: &str,
    ) -> Result<(Option<tempfile::TempDir>, Vec<Mount>)> {
        let mut mounts = Vec::new();
        let temp_dir = if container.volume_mounts.is_some() {
            Some(
                tempfile::Builder::new()
                    .prefix(&format!("{}-volumes-", container_name))
                    .tempdir()?,
            )
        } else {
            None
        };

        if let (Some(volume_mounts), Some(volumes)) = (&container.volume_mounts, &container.volumes)
        {
            for mount in volume_mounts {
                if let Some(volume_data) = volumes.get(&mount.name) {
                    if let Some(host_path) = &volume_data.host_path {
                        mounts.push(Mount {
                            target: Some(mount.mount_path.clone()),
                            source: Some(host_path.clone()),
                            typ: Some(MountTypeEnum::BIND),
                            ..Default::default()
                        });
                    } else if let Some(files) = &volume_data.files {
                        let temp_dir = temp_dir.as_ref().expect("Temp dir should exist");
                        let volume_dir = temp_dir.path().join(&mount.name);
                        tokio::fs::create_dir_all(&volume_dir).await?;

                        for (filename, content) in files {
                            let file_path = if let Some(sub_path) = &mount.sub_path {
                                if sub_path == filename {
                                    volume_dir.join(filename)
                                } else {
                                    continue;
                                }
                            } else {
                                volume_dir.join(filename)
                            };

                            tokio::fs::write(&file_path, content).await?;

                            let mount_target = if mount.sub_path.is_some() {
                                mount.mount_path.clone()
                            } else {
                                format!("{}/{}", mount.mount_path, filename)
                            };

                            mounts.push(Mount {
                                target: Some(mount_target),
                                source: Some(file_path.to_string_lossy().into_owned()),
                                typ: Some(MountTypeEnum::BIND),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        Ok((temp_dir, mounts))
    }

    async fn prepare_port_configuration(
        &self,
        container: &Container,
    ) -> Result<(
        HashMap<String, Option<Vec<PortBinding>>>,
        HashMap<String, HashMap<(), ()>>,
        Vec<ContainerPortMetadata>,
    )> {
        let mut port_bindings = HashMap::new();
        let mut exposed_ports = HashMap::new();
        let mut assigned_port_metadata = Vec::new();

        if let Some(ports) = &container.ports {
            for port_config in ports {
                let container_port = port_config.target_port.unwrap_or(port_config.port);
                let container_port_key = format!("{}/tcp", container_port);

                let host_port = Self::find_available_port()
                    .ok_or_else(|| anyhow!("No available ports found in the configured range"))?;

                port_bindings.insert(
                    container_port_key.clone(),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: Some(host_port.to_string()),
                    }]),
                );

                exposed_ports.insert(container_port_key, HashMap::new());
                assigned_port_metadata.push(ContainerPortMetadata {
                    port: host_port,
                    node_port: port_config.node_port.unwrap_or(port_config.port),
                });
            }
        }

        Ok((port_bindings, exposed_ports, assigned_port_metadata))
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn start_containers(
        &self,
        service_name: &str,
        pod_number: u8,
        containers: &Vec<Container>,
        memory_limit: Option<Value>,
        cpu_limit: Option<Value>,
    ) -> Result<Vec<(String, Vec<ContainerPortMetadata>)>> {
        let uuid = Uuid::new_v4();
        let mut started_containers = Vec::new();
        let mut containers_to_cleanup = Vec::new();

        // Parse resource limits once as they apply to all containers in the pod
        let parsed_memory_limit = memory_limit
            .as_ref()
            .map(|v| parse_memory_limit(v).unwrap_or(0))
            .unwrap_or(0);
        let parsed_cpu_limit = cpu_limit
            .as_ref()
            .map(|v| parse_cpu_limit(v).unwrap_or(0))
            .unwrap_or(0);

        // Track creation success
        let mut pod_creation_failed = false;

        for container in containers {
            if pod_creation_failed {
                break;
            }
            let container_name =
                container.generate_runtime_name(service_name, pod_number, &uuid.to_string())?;

            // Setup volume mounts
            let (_temp_dir, mounts) =
                match self.setup_volume_mounts(container, &container_name).await {
                    Ok(result) => result,
                    Err(e) => {
                        slog::error!(slog_scope::logger(), "Failed to setup volume mounts";
                            "service" => service_name,
                            "container" => &container_name,
                            "error" => e.to_string()
                        );
                        pod_creation_failed = true;
                        break;
                    }
                };
            // Prepare port bindings

            // Prepare port bindings and metadata
            let (port_bindings, exposed_ports, assigned_port_metadata) =
                match self.prepare_port_configuration(container).await {
                    Ok(result) => result,
                    Err(e) => {
                        slog::error!(slog_scope::logger(), "Failed to prepare port configuration";
                            "service" => service_name,
                            "container" => &container_name,
                            "error" => e.to_string()
                        );
                        pod_creation_failed = true;
                        break;
                    }
                };

            // Create container config with both ports and volumes
            let mut host_config = HostConfig {
                port_bindings: Some(port_bindings),
                memory: Some(parsed_memory_limit.try_into().unwrap()),
                nano_cpus: Some(parsed_cpu_limit.try_into().unwrap()),
                ..Default::default()
            };

            if !mounts.is_empty() {
                host_config.mounts = Some(mounts);
            }

            let config = Config {
                image: Some(container.image.clone()),
                host_config: Some(host_config),
                exposed_ports: Some(exposed_ports),
                ..Default::default()
            };

            // Create and start the container
            match self
                .client
                .create_container(
                    Some(CreateContainerOptions {
                        name: container_name.as_str(),
                        platform: None,
                    }),
                    config,
                )
                .await
            {
                Ok(_) => {
                    match self
                        .client
                        .start_container(&container_name, None::<StartContainerOptions<String>>)
                        .await
                    {
                        Ok(_) => {
                            slog::info!(slog_scope::logger(), "Container started successfully";
                                "service" => service_name,
                                "container" => &container_name,
                                "ports" => ?assigned_port_metadata
                            );
                            containers_to_cleanup.push(container_name.clone());
                            started_containers.push((container_name, assigned_port_metadata));
                        }
                        Err(e) => {
                            slog::error!(slog_scope::logger(), "Failed to start container";
                                "service" => service_name,
                                "container" => &container_name,
                                "error" => e.to_string()
                            );
                            pod_creation_failed = true;
                            break;
                        }
                    }
                }
                Err(e) => {
                    slog::error!(slog_scope::logger(), "Failed to create container";
                        "service" => service_name,
                        "container" => &container_name,
                        "error" => e.to_string()
                    );
                    pod_creation_failed = true;
                    break;
                }
            }
        }

        // If any container failed to start, clean up all containers in the pod
        if pod_creation_failed {
            for container_name in containers_to_cleanup {
                if let Err(e) = self.stop_container(&container_name).await {
                    slog::error!(slog_scope::logger(), "Failed to cleanup container after pod creation failure";
                        "service" => &service_name,
                        "container" => &container_name,
                        "error" => e.to_string()
                    );
                }
            }
            return Err(anyhow!("Failed to create one or more containers in pod"));
        }

        Ok(started_containers)
    }

    async fn stop_container(&self, name: &str) -> Result<()> {
        // Stop the container
        self.client
            .stop_container(name, None)
            .await
            .map_err(|e| anyhow!("Failed to stop container {}: {:?}", name, e))?;

        // Remove the container
        self.client
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| anyhow!("Failed to remove container {}: {:?}", name, e))?;

        Ok(())
    }

    async fn inspect_container(&self, name: &str) -> Result<ContainerStats> {
        let options = Some(StatsOptions {
            stream: false,
            one_shot: true,
        });

        let mut stats_stream = self.client.stats(name, options);

        let stats = stats_stream
            .next()
            .await
            .ok_or_else(|| anyhow!("No stats available for container {}", name))?;

        let stats = stats?;

        // Get container inspection data for port mappings
        let container_data = self.client.inspect_container(name, None).await?;

        // Extract port mappings from container data
        let mut port_mappings = HashMap::new();
        if let Some(network_settings) = container_data.network_settings {
            if let Some(ports) = network_settings.ports {
                for (container_port_proto, host_bindings) in ports {
                    if let Some(host_bindings) = host_bindings {
                        for binding in host_bindings {
                            if let Some(host_port) = binding.host_port {
                                // Parse "80/tcp" to get just the port number
                                if let Some(container_port) = container_port_proto
                                    .split('/')
                                    .next()
                                    .and_then(|p| p.parse::<u16>().ok())
                                {
                                    if let Ok(host_port) = host_port.parse::<u16>() {
                                        port_mappings.insert(container_port, host_port);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let service_name = name
            .splitn(2, "__")
            .next()
            .expect("Split always returns at least one element");

        let service_cfg = get_config_by_service(service_name).unwrap();

        let nano_cpus = service_cfg
            .cpu_limit
            .as_ref() // Safely access the Option<Value>
            .and_then(|value| parse_cpu_limit(value).ok()); // Parse and handle Result -> Option

        // Update stats history and get CPU percentage
        let mut container_stats =
            update_container_stats(service_name, name, stats.clone(), nano_cpus).await;
        container_stats.port_mappings = port_mappings;

        Ok(container_stats)
    }

    async fn list_containers(&self, service_name: Option<&str>) -> Result<Vec<ContainerInfo>> {
        let mut filters = HashMap::new();

        if let Some(service_name) = service_name {
            // Filter by service name using the naming convention
            filters.insert("name".to_string(), vec![format!("{}__", service_name)]);
        }

        let containers = self
            .client
            .list_containers(Some(bollard::container::ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await?;

        Ok(containers
            .into_iter()
            .map(|c| ContainerInfo {
                id: c.id.unwrap_or_default(),
                name: c
                    .names
                    .unwrap_or_default()
                    .into_iter()
                    .map(|name| name.trim_start_matches('/').to_string())
                    .next()
                    .unwrap_or_default(),
                state: c.state.unwrap_or_default(),
                port: c
                    .ports
                    .unwrap_or_default()
                    .get(0)
                    .and_then(|p| p.public_port) // Flatten the nested Option
                    .unwrap_or(0), // Default to 0 if no port is available
            })
            .collect())
    }
}

pub fn create_runtime(runtime: &str) -> Result<Arc<dyn ContainerRuntime>> {
    match runtime {
        "docker" => Ok(Arc::new(DockerRuntime::new()?)),
        _ => Err(anyhow!("Unsupported runtime: {}", runtime)),
    }
}

pub async fn get_next_pod_number(service_name: &str) -> u8 {
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();

    match runtime.list_containers(Some(service_name)).await {
        Ok(containers) => containers
            .iter()
            .filter_map(|c| parse_container_name(&c.name).ok())
            .map(|parts| parts.pod_number)
            .max()
            .map_or(0, |max| max + 1),
        Err(_) => 0,
    }
}

pub async fn manage(service_name: &str, config: ServiceConfig) {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();

    // Calculate required containers outside the lock
    let current_instances = {
        let instances = instance_store
            .get(service_name)
            .map(|entry| entry.value().len())
            .unwrap_or(0);
        instances
    };

    let target_instances = config.instance_count.min as usize;

    // Scale up without holding the main lock
    if current_instances < target_instances {
        slog::debug!(log, "Starting scale up";
            "service" => service_name,
            "current" => current_instances,
            "target" => target_instances
        );

        for _ in current_instances..target_instances {
            let pod_number = get_next_pod_number(service_name).await;

            let uuid = uuid::Uuid::new_v4();

            slog::debug!(log, "Starting new pod instance";
                "service" => service_name,
                "pod_number" => pod_number,
                "uuid" => uuid.to_string()
            );

            match runtime
                .start_containers(
                    service_name,
                    pod_number as u8, // Pass the pod number
                    &config.spec.containers,
                    config.memory_limit.clone(),
                    config.cpu_limit.clone(),
                )
                .await
            {
                Ok(started_containers) => {
                    for (container_name, ports) in &started_containers {
                        slog::debug!(log, "Container started successfully";
                            "service" => service_name,
                            "container" => container_name,
                            "ports" => ?ports
                        );
                    }

                    // Only lock briefly to update the instance store
                    if let Some(mut instances) = instance_store.get_mut(service_name) {
                        instances.insert(
                            uuid,
                            InstanceMetadata {
                                uuid,
                                containers: started_containers
                                    .into_iter()
                                    .map(|(name, ports)| ContainerMetadata {
                                        name,
                                        ports,
                                        status: "running".to_string(),
                                    })
                                    .collect(),
                            },
                        );
                    } else {
                        // Create new entry if it doesn't exist
                        let mut map = HashMap::new();
                        map.insert(
                            uuid,
                            InstanceMetadata {
                                uuid,
                                containers: started_containers
                                    .into_iter()
                                    .map(|(name, ports)| ContainerMetadata {
                                        name,
                                        ports,
                                        status: "running".to_string(),
                                    })
                                    .collect(),
                            },
                        );
                        instance_store.insert(service_name.to_string(), map);
                    }

                    tokio::task::yield_now().await;
                }
                Err(e) => {
                    slog::error!(log, "Failed to start containers";
                        "service" => service_name,
                        "error" => e.to_string()
                    );
                }
            }
        }
    }
}

pub async fn clean_up(service_name: &str) {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised");
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    // Stop the auto-scaling task
    if let Some((_, handle)) = scaling_tasks.remove(service_name) {
        handle.abort();
        slog::trace!(log, "Scaling task aborted"; "service" => service_name);
    }

    if let Some((_, instances)) = instance_store.remove(service_name) {
        for (_uuid, metadata) in instances {
            // For each container in the pod
            for container in metadata.containers {
                // Remove from load balancer for each port
                for port_metadata in &container.ports {
                    // Need to remove from the specific proxy instance for this node_port
                    let proxy_key = format!("{}_{}", service_name, port_metadata.node_port);
                    if let Some(backends) = SERVER_BACKENDS.get().unwrap().get(&proxy_key) {
                        let addr = format!("127.0.0.1:{}", port_metadata.port);
                        if let Ok(backend) = Backend::new(&addr) {
                            backends.remove(&backend);
                            slog::debug!(log, "Removed backend from load balancer";
                                "service" => service_name,
                                "container" => &container.name,
                                "port" => port_metadata.port,
                                "node_port" => port_metadata.node_port
                            );
                        }
                    }
                }
                // Clean up stats for each container
                remove_container_stats(service_name, &container.name);

                // Stop each container
                let runtime = runtime.clone();
                if let Err(e) = runtime.stop_container(&container.name).await {
                    slog::error!(log, "Failed to stop container";
                        "service" => service_name,
                        "container" => &container.name,
                        "error" => e.to_string()
                    );
                }
            }
        }

        // Clean up entire service stats after all containers are stopped
        if let Some(service_stats) = SERVICE_STATS.get() {
            service_stats.remove(service_name);
        }
    }

    update_instance_store_cache();
}

// Helper function to calculate CPU percentages
fn calculate_cpu_percentages(
    previous: Option<&StatsEntry>,
    cpu_total: u64,
    system_cpu: u64,
    online_cpus: f64,
    nano_cpus: Option<u64>,
) -> (f64, f64) {
    if let Some(previous) = previous {
        let cpu_delta = cpu_total as f64 - previous.cpu_total_usage as f64;
        let system_delta = system_cpu as f64 - previous.system_cpu_usage as f64;

        if system_delta > 0.0 && cpu_delta >= 0.0 {
            // Calculate absolute CPU percentage (across all cores)
            let absolute_cpu = ((cpu_delta / system_delta) * online_cpus * 100.0)
                .max(0.0)
                .min(100.0 * online_cpus);

            // Calculate relative CPU percentage
            let relative_cpu = if let Some(cpu_limit) = nano_cpus {
                // Convert nanocpus to CPU cores (1 CPU = 1_000_000_000 nanocpus)
                let allocated_cpu = cpu_limit as f64 / 1_000_000_000.0;
                if allocated_cpu > 0.0 {
                    // Calculate relative to allocated CPU
                    // Since absolute_cpu is across all cores, we need to compare with allocated_cpu * 100
                    let relative = (absolute_cpu / online_cpus) / allocated_cpu;
                    // Convert to percentage and clamp between 0-100
                    (relative * 100.0).max(0.0).min(100.0)
                } else {
                    0.0 // Avoid division by zero
                }
            } else {
                absolute_cpu / online_cpus // Normalize by number of CPUs if no limit
            };

            slog::trace!(slog_scope::logger(), "CPU calculation details";
                "cpu_delta" => cpu_delta,
                "system_delta" => system_delta,
                "absolute_cpu" => absolute_cpu,
                "relative_cpu" => relative_cpu,
                "online_cpus" => online_cpus,
                "allocated_cpu" => nano_cpus.map(|n| n as f64 / 1_000_000_000.0).unwrap_or(0.0)
            );

            (absolute_cpu / online_cpus, relative_cpu) // Normalize absolute CPU by cores
        } else {
            (0.0, 0.0)
        }
    } else {
        (0.0, 0.0) // First reading
    }
}
