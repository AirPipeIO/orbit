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
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::SystemTime;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{parse_cpu_limit, parse_memory_limit, ServiceConfig, CONFIG_STORE};
use crate::proxy::SERVER_BACKENDS;
use crate::status::update_instance_store_cache;

pub static RUNTIME: OnceLock<Arc<dyn ContainerRuntime>> = OnceLock::new();

pub static RESERVED_PORTS: OnceLock<DashMap<u16, String>> = OnceLock::new();

pub static INSTANCE_STORE: OnceLock<DashMap<String, HashMap<Uuid, InstanceMetadata>>> =
    OnceLock::new();

// Global registry for scaling tasks
pub static SCALING_TASKS: OnceLock<DashMap<String, JoinHandle<()>>> = OnceLock::new();

// Global stats history store
#[derive(Clone)]
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
        port_range: std::ops::Range<u16>,
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

    fn find_available_port(mut port_range: std::ops::Range<u16>) -> Option<u16> {
        port_range.find(|port: &u16| Self::is_port_available(*port))
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn start_container(
        &self,
        name: &str,
        image: &str,
        target_port: u16,
        port_range: std::ops::Range<u16>,
        memory_limit: Option<Value>, // Human-readable memory limit (e.g., "512Mi")
        cpu_limit: Option<Value>,    // Human-readable CPU limit (e.g., "0.5")
    ) -> Result<u16> {
        // Find an available port in the range
        let available_port = Self::find_available_port(port_range)
            .ok_or_else(|| anyhow!("No available ports found in the specified range"))?;

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

        let service_name = name.splitn(2, "__").next().unwrap_or("");
        let config_store = CONFIG_STORE.get().unwrap();
        let config_file = config_store.iter().find_map(|entry| {
            let service_config = entry.value();
            if service_config.name == service_name {
                Some(entry.key().clone())
            } else {
                None
            }
        });

        let service_cfg = config_store.get(&config_file.unwrap()).unwrap();

        let nano_cpus = service_cfg
            .cpu_limit
            .as_ref() // Safely access the Option<Value>
            .and_then(|value| parse_cpu_limit(value).ok()); // Parse and handle Result -> Option

        // Update stats history and get CPU percentage
        let cpu = update_container_stats(name, stats.clone(), nano_cpus);

        let memory_usage = stats.memory_stats.usage.unwrap_or(0);
        let memory_limit = stats.memory_stats.limit.unwrap_or(0);

        Ok(ContainerStats {
            id: stats.id,
            cpu_percentage: cpu.cpu_percentage,
            cpu_percentage_relative: cpu.cpu_percentage_relative,
            memory_usage,
            memory_limit,
        })
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
    let cfg: ServiceConfig = config.clone();

    let mut instances = instance_store.entry(service_name.to_string()).or_default();

    // Start additional containers if necessary
    for _ in instances.len()..(cfg.instance_count.min as usize) {
        let uuid = uuid::Uuid::new_v4();
        let container_name = format!("{}__{}", service_name, uuid);
        let port_range = cfg.exposed_port..cfg.exposed_port + 10;

        match runtime
            .start_container(
                &container_name,
                &cfg.image,
                cfg.target_port,
                port_range,
                cfg.memory_limit.clone(),
                cfg.cpu_limit.clone(),
            )
            .await
        {
            Ok(exposed_port) => {
                instances.insert(
                    uuid,
                    InstanceMetadata {
                        uuid,
                        exposed_port,
                        status: "running".to_string(),
                    },
                );
            }
            Err(e) => {
                slog::error!(log, "Failed to start container";
                    "service" => service_name,
                    "error" => e.to_string()
                );
            }
        }
    }

    // Stop extra containers if necessary
    while instances.len() > cfg.instance_count.min as usize {
        if let Some((&uuid, metadata)) = instances.iter().next() {
            let container_name = format!("{}__{}", service_name, uuid);
            let exposed_port = metadata.exposed_port;

            // Step 1: Remove backend from load balancer
            let server_backends = SERVER_BACKENDS.get().unwrap();
            if let Some(backends) = server_backends.get(service_name) {
                let addr = format!("127.0.0.1:{}", exposed_port);
                backends.remove(&Backend::new(&addr).unwrap());
                slog::trace!(log, "Removed backend from load balancer";
                    "service" => service_name,
                    "container" => container_name.clone(),
                    "address" => addr
                );
            }

            // Step 2: Graceful wait (e.g., 5 seconds)
            tokio::time::sleep(Duration::from_secs(5)).await;

            // Step 3: Stop the container
            if runtime.stop_container(&container_name).await.is_ok() {
                instances.remove(&uuid);
            }
        }
    }

    // seems to lock if this enabled
    // update_instance_store_cache();
}

pub async fn clean_up(service_name: &str) {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised");
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();
    let server_backends = SERVER_BACKENDS.get().unwrap();
    let scaling_tasks = SCALING_TASKS.get().unwrap(); // Access the global scaling tasks

    // Stop the auto-scaling task
    if let Some((_, handle)) = scaling_tasks.remove(service_name) {
        handle.abort(); // Abort the scaling task
        slog::trace!(log, "Scaling task aborted"; "service" => service_name);
    }

    // Clone the instances to process outside of the DashMap
    let instances = {
        let mut instance_store = instance_store;
        instance_store
            .remove(service_name)
            .map(|(_, instances)| instances)
    };

    // Stop and remove container instances
    if let Some(instances) = instances {
        let service_name_owned = service_name.to_string();
        let backends = server_backends
            .get(&service_name_owned)
            .map(|entry| entry.clone()); // Clone the Arc<DashSet<Backend>>

        let tasks: Vec<_> = instances
            .into_iter()
            .map(|(uuid, metadata)| {
                let container_name = format!("{}__{}", service_name_owned.clone(), uuid);
                let runtime = runtime.clone();
                let service_name_for_task = service_name_owned.clone();
                let backends = backends.clone();
                let exposed_port = metadata.exposed_port;

                // let container_name = format!("{}__{}", service_name, uuid);
                // Remove container stats when cleaning up
                remove_container_stats(&container_name);

                tokio::spawn(async move {
                    let log = slog_scope::logger();

                    // Remove backend from the load balancer
                    if let Some(backends) = backends {
                        let addr = format!("127.0.0.1:{}", exposed_port);
                        backends.remove(&Backend::new(&addr).unwrap());
                        slog::trace!(log, "Removed backend from load balancer";
                            "service" => service_name_for_task.clone(),
                            "container" => container_name.clone(),
                            "address" => addr
                        );
                    }

                    // Graceful wait (e.g., 5 seconds) for active connections to complete
                    tokio::time::sleep(Duration::from_secs(5)).await;

                    // Stop the container
                    if let Err(e) = runtime.stop_container(&container_name).await {
                        slog::error!(log, "Failed to stop container";
                            "service" => service_name_for_task,
                            "container" => container_name,
                            "error" => e.to_string()
                        );
                    } else {
                        slog::trace!(log, "Stopped container";
                            "service" => service_name_for_task,
                            "container" => container_name
                        );
                    }
                })
            })
            .collect();

        // Wait for all tasks to complete
        for task in tasks {
            if let Err(e) = task.await {
                slog::error!(slog_scope::logger(), "Error in container stop task";
                    "error" => e.to_string()
                );
            }
        }
    }

    // Update the instance store cache
    update_instance_store_cache();
}

pub fn update_container_stats(
    container_name: &str,
    stats: Stats,
    nano_cpus: Option<u64>,
) -> ContainerStats {
    let stats_store = CONTAINER_STATS
        .get()
        .expect("Stats tracker not initialized");
    let now = SystemTime::now();

    // Get current values
    let cpu_total = stats.cpu_stats.cpu_usage.total_usage;
    let system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
    let online_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;

    // Calculate CPU percentages
    let (cpu_percentage, cpu_percentage_relative) =
        if let Some(previous) = stats_store.get(container_name) {
            let cpu_delta = cpu_total as f64 - previous.cpu_total_usage as f64;
            let system_delta = system_cpu as f64 - previous.system_cpu_usage as f64;

            if system_delta > 0.0 && cpu_delta > 0.0 {
                let absolute_cpu = ((cpu_delta / system_delta) * online_cpus * 100.0)
                    .max(0.0)
                    .min(100.0 * online_cpus);

                // Calculate relative CPU usage based on allocated CPU limit
                let relative_cpu = if let Some(cpu_limit) = nano_cpus {
                    let allocated_cpu = (cpu_limit as f64 / 1_000_000_000.0) * 100.0;
                    (absolute_cpu / allocated_cpu * 100.0).min(100.0)
                } else {
                    absolute_cpu
                };

                (absolute_cpu, relative_cpu)
            } else {
                (0.0, 0.0)
            }
        } else {
            (0.0, 0.0)
        };

    // Store current values for next calculation
    stats_store.insert(
        container_name.to_string(),
        StatsEntry {
            timestamp: now,
            cpu_total_usage: cpu_total,
            system_cpu_usage: system_cpu,
        },
    );

    // println!(
    //     "\n\n {:?} - cpu rel: {:?}, cpu: {:?}",
    //     container_name.to_string(),
    //     cpu_percentage_relative,
    //     cpu_percentage
    // );

    ContainerStats {
        id: stats.id,
        cpu_percentage,
        cpu_percentage_relative,
        memory_usage: stats.memory_stats.usage.unwrap_or(0),
        memory_limit: stats.memory_stats.limit.unwrap_or(0),
    }
}

pub fn update_container_stats_old(container_name: &str, stats: Stats) -> f64 {
    let stats_store = CONTAINER_STATS
        .get()
        .expect("Stats tracker not initialized");
    let now = SystemTime::now();

    // Get current values
    let cpu_total = stats.cpu_stats.cpu_usage.total_usage;
    let system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
    let online_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;

    // Calculate CPU percentage using previous values if they exist
    let cpu_percentage = if let Some(previous) = stats_store.get(container_name) {
        let cpu_delta = cpu_total as f64 - previous.cpu_total_usage as f64;
        let system_delta = system_cpu as f64 - previous.system_cpu_usage as f64;

        if system_delta > 0.0 && cpu_delta > 0.0 {
            ((cpu_delta / system_delta) * online_cpus * 100.0)
                .max(0.0)
                .min(100.0 * online_cpus)
        } else {
            0.0
        }
    } else {
        0.0 // First reading, can't calculate percentage yet
    };

    // Store current values for next calculation
    stats_store.insert(
        container_name.to_string(),
        StatsEntry {
            timestamp: now,
            cpu_total_usage: cpu_total,
            system_cpu_usage: system_cpu,
        },
    );

    cpu_percentage
}
pub fn remove_container_stats(container_name: &str) {
    if let Some(stats_store) = CONTAINER_STATS.get() {
        stats_store.remove(container_name);
    }
}
