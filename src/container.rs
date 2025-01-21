//container.rs
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, Stats,
    StatsOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::Docker;
use dashmap::DashMap;
use futures::StreamExt;
use pingora_load_balancing::Backend;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::SystemTime;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{get_config_by_service, parse_cpu_limit, parse_memory_limit, ServiceConfig};
use crate::proxy::SERVER_BACKENDS;
use crate::status::update_instance_store_cache;

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
        timestamp: now,
    };

    // Update service-level stats
    service_stats
        .entry(service_name.to_string())
        .or_insert_with(ServiceStats::new)
        .update_stats(container_name, container_stats.clone());

    container_stats
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
    pub uuid: Uuid,        // Unique identifier for the instance
    pub exposed_port: u16, // Port mapped for this container
    pub status: String,    // Status (e.g., "running", "stopped")
}

// Define the container runtime trait
#[async_trait]
pub trait ContainerRuntime: Send + Sync + std::fmt::Debug {
    async fn start_container(
        &self,
        name: &str,
        image: &str,
        target_port: u16,
        memory_limit: Option<Value>, // Human-readable memory limit (e.g., "512Mi")
        cpu_limit: Option<Value>,    // Human-readable CPU limit (e.g., "0.5")
    ) -> Result<u16>;
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
    timestamp: SystemTime,
}

pub fn update_reserved_ports(service_name: &str, exposed_port: u16) {
    let reserved_ports = RESERVED_PORTS.get().unwrap();
    reserved_ports.insert(exposed_port, service_name.to_string());
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
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn start_container(
        &self,
        name: &str,
        image: &str,
        target_port: u16,
        memory_limit: Option<Value>, // Human-readable memory limit (e.g., "512Mi")
        cpu_limit: Option<Value>,    // Human-readable CPU limit (e.g., "0.5")
    ) -> Result<u16> {
        // Find an available port using the globally configured range
        let available_port = Self::find_available_port()
            .ok_or_else(|| anyhow!("No available ports found in the configured range"))?;

        // Container configuration
        // Parse the memory and CPU limits
        let parsed_memory_limit = memory_limit
            .as_ref()
            .map(|v| parse_memory_limit(v).unwrap_or(0))
            .unwrap_or(0);
        let parsed_cpu_limit = cpu_limit
            .as_ref()
            .map(|v| parse_cpu_limit(v).unwrap_or(0))
            .unwrap_or(0);

        // Container configuration
        let config = Config {
            image: Some(image.to_string()),
            host_config: Some(HostConfig {
                port_bindings: Some(HashMap::from([(
                    format!("{}/tcp", target_port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: Some(available_port.to_string()),
                    }]),
                )])),
                memory: Some(parsed_memory_limit.try_into().unwrap()), // Pass parsed memory limit
                nano_cpus: Some(parsed_cpu_limit.try_into().unwrap()), // Pass parsed CPU limit
                ..Default::default()
            }),
            ..Default::default()
        };

        // Create the container
        self.client
            .create_container(
                Some(CreateContainerOptions {
                    name,
                    platform: None,
                }),
                config,
            )
            .await
            .map_err(|e| anyhow!("Failed to create container {}: {:?}", name, e))?;

        // Start the container
        self.client
            .start_container(name, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| anyhow!("Failed to start container {}: {:?}", name, e))?;

        Ok(available_port)
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
        let container_stats =
            update_container_stats(service_name, name, stats.clone(), nano_cpus).await;

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
            let uuid = uuid::Uuid::new_v4();
            let container_name = format!("{}__{}", service_name, uuid);

            slog::debug!(log, "Starting new container";
                "service" => service_name,
                "container" => &container_name
            );

            match runtime
                .start_container(
                    &container_name,
                    &config.image,
                    config.target_port,
                    config.memory_limit.clone(),
                    config.cpu_limit.clone(),
                )
                .await
            {
                Ok(exposed_port) => {
                    slog::debug!(log, "Container started successfully";
                        "service" => service_name,
                        "container" => &container_name,
                        "port" => exposed_port.to_string()
                    );

                    // Only lock briefly to update the instance store
                    if let Some(mut instances) = instance_store.get_mut(service_name) {
                        instances.insert(
                            uuid,
                            InstanceMetadata {
                                uuid,
                                exposed_port,
                                status: "running".to_string(),
                            },
                        );
                    } else {
                        // Create new entry if it doesn't exist
                        let mut map = HashMap::new();
                        map.insert(
                            uuid,
                            InstanceMetadata {
                                uuid,
                                exposed_port,
                                status: "running".to_string(),
                            },
                        );
                        instance_store.insert(service_name.to_string(), map);
                    }

                    tokio::task::yield_now().await;
                }
                Err(e) => {
                    slog::error!(log, "Failed to start container";
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
        for (uuid, metadata) in instances {
            let container_name = format!("{}__{}", service_name, uuid);

            // Remove from load balancer
            if let Some(backends) = SERVER_BACKENDS.get().unwrap().get(service_name) {
                let addr = format!("127.0.0.1:{}", metadata.exposed_port);
                if let Ok(backend) = Backend::new(&addr) {
                    backends.remove(&backend);
                }
            }

            // Clean up stats first
            remove_container_stats(service_name, &container_name);

            // Stop the container
            let runtime = runtime.clone();
            if let Err(e) = runtime.stop_container(&container_name).await {
                slog::error!(log, "Failed to stop container";
                    "service" => service_name,
                    "container" => &container_name,
                    "error" => e.to_string()
                );
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
