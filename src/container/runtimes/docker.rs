// src/container/runtimes/docker.rs
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StatsOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::network::CreateNetworkOptions;
use bollard::secret::{DeviceRequest, Mount, MountBindOptions, MountTypeEnum};
use bollard::Docker;
use dashmap::DashMap;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

use crate::config::{get_config_by_service, parse_cpu_limit, parse_memory_limit, ServiceConfig};
use crate::container::{
    parse_network_rate, update_container_stats, Container, ContainerInfo, ContainerPortMetadata,
    ContainerRuntime, ContainerStats, NetworkLimit,
};

use super::NETWORK_USAGE;

#[derive(Debug, Clone)]
pub struct DockerRuntime {
    client: Docker,
}

impl DockerRuntime {
    pub fn new() -> Result<Self> {
        let client = Docker::connect_with_local_defaults()
            .map_err(|e| anyhow!("Failed to connect to Docker: {:?}", e))?;
        Ok(Self { client })
    }

    async fn track_network_usage(&self, network_name: &str, service_name: &str) {
        let network_usage = NETWORK_USAGE.get_or_init(DashMap::new);
        network_usage
            .entry(network_name.to_string())
            .or_insert_with(HashSet::new)
            .insert(service_name.to_string());
    }

    async fn untrack_network_usage(&self, network_name: &str, service_name: &str) -> bool {
        let network_usage = NETWORK_USAGE.get().expect("Network usage not initialized");

        if let Some(mut services) = network_usage.get_mut(network_name) {
            services.remove(service_name);
            if services.is_empty() {
                network_usage.remove(network_name);
                return true; // Network has no more users
            }
        }
        false
    }

    async fn setup_pod_network(
        &self,
        service_name: &str,
        uuid: &str,
        container_count: usize,
        config: &ServiceConfig,
    ) -> Result<Option<String>> {
        if let Some(network_name) = &config.network {
            if let Ok(networks) = self.client.list_networks::<String>(None).await {
                if !networks
                    .iter()
                    .any(|n| n.name == Some(network_name.clone()))
                {
                    self.client
                        .create_network(CreateNetworkOptions {
                            name: network_name.clone(),
                            driver: "bridge".to_string(),
                            ..Default::default()
                        })
                        .await?;
                }
            }
            // Track usage of user-defined network
            self.track_network_usage(network_name, service_name).await;
            return Ok(Some(network_name.clone()));
        }

        if container_count <= 1 {
            return Ok(None);
        }

        // For multi-container pods without specified network, create dedicated network
        let network_name = format!("{}__{}", service_name, uuid);

        // Check if network exists and remove if it does
        if let Ok(networks) = self.client.list_networks::<String>(None).await {
            if networks
                .iter()
                .any(|n| n.name == Some(network_name.clone()))
            {
                self.client.remove_network(&network_name).await?;
            }
        }

        // Create network
        self.client
            .create_network(CreateNetworkOptions {
                name: network_name.clone(),
                driver: "bridge".to_string(),
                ..Default::default()
            })
            .await?;

        Ok(Some(network_name))
    }

    async fn setup_volume_mounts(
        &self,
        container: &Container,
        container_name: &str,
        config: &ServiceConfig,
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

        if let (Some(volume_mounts), Some(volumes)) = (&container.volume_mounts, &config.volumes) {
            for mount in volume_mounts {
                if let Some(volume_data) = volumes.get(&mount.name) {
                    if let Some(host_path) = &volume_data.host_path {
                        let host_path = Path::new(host_path);
                        if !host_path.exists() {
                            return Err(anyhow!("Host path does not exist: {:?}", host_path));
                        }

                        slog::info!(slog_scope::logger(), "Setting up host path mount";
                            "container" => container_name,
                            "host_path" => host_path.display().to_string(),
                            "mount_path" => &mount.mount_path
                        );

                        mounts.push(Mount {
                            target: Some(mount.mount_path.clone()),
                            source: Some(host_path.to_string_lossy().into_owned()),
                            typ: Some(MountTypeEnum::BIND),
                            read_only: Some(mount.read_only.unwrap_or(true)),
                            bind_options: Some(MountBindOptions {
                                propagation: None,
                                non_recursive: Some(false),
                                create_mountpoint: Some(false),
                                read_only_force_recursive: Some(false),
                                read_only_non_recursive: Some(false),
                            }),
                            volume_options: None,
                            tmpfs_options: None,
                            consistency: Some("default".to_string()),
                        });
                    } else if let Some(files) = &volume_data.files {
                        let temp_dir = temp_dir.as_ref().expect("Temp dir should exist");
                        let volume_dir = temp_dir.path().join(&mount.name);
                        tokio::fs::create_dir_all(&volume_dir).await?;

                        for (filename, content) in files {
                            let file_path = volume_dir.join(filename);
                            tokio::fs::write(&file_path, content).await?;
                        }

                        if let Some(sub_path) = &mount.sub_path {
                            let source_file = volume_dir.join(sub_path);
                            if !source_file.exists() {
                                return Err(anyhow!("Subpath file {} not found", sub_path));
                            }

                            mounts.push(Mount {
                                target: Some(mount.mount_path.clone()),
                                source: Some(source_file.to_string_lossy().into_owned()),
                                typ: Some(MountTypeEnum::BIND),
                                read_only: Some(mount.read_only.unwrap_or(true)),
                                bind_options: Some(MountBindOptions {
                                    propagation: None,
                                    non_recursive: Some(false),
                                    create_mountpoint: Some(false),
                                    read_only_force_recursive: Some(false),
                                    read_only_non_recursive: Some(false),
                                }),
                                volume_options: None,
                                tmpfs_options: None,
                                consistency: Some("default".to_string()),
                            });
                        } else {
                            mounts.push(Mount {
                                target: Some(mount.mount_path.clone()),
                                source: Some(volume_dir.to_string_lossy().into_owned()),
                                typ: Some(MountTypeEnum::BIND),
                                read_only: Some(mount.read_only.unwrap_or(true)),
                                bind_options: Some(MountBindOptions {
                                    propagation: None,
                                    non_recursive: Some(false),
                                    create_mountpoint: Some(false),
                                    read_only_force_recursive: Some(false),
                                    read_only_non_recursive: Some(false),
                                }),
                                volume_options: None,
                                tmpfs_options: None,
                                consistency: Some("default".to_string()),
                            });
                        }
                    }
                }
            }
        }

        Ok((temp_dir, mounts))
    }

    fn prepare_network_limits(&self, network_limit: &NetworkLimit) -> Result<Vec<DeviceRequest>> {
        let mut device_requests = Vec::new();

        if let (Some(ingress), Some(egress)) =
            (&network_limit.ingress_rate, &network_limit.egress_rate)
        {
            // Convert rates to bits per second
            let ingress_bps = parse_network_rate(ingress)?;
            let egress_bps = parse_network_rate(egress)?;

            // Create TC (traffic control) rules
            device_requests.push(DeviceRequest {
                driver: Some("tc-ingress".to_string()),
                count: Some(1),
                capabilities: Some(vec![vec![format!("rate={}", ingress_bps)]]),
                ..Default::default()
            });

            device_requests.push(DeviceRequest {
                driver: Some("tc-egress".to_string()),
                count: Some(1),
                capabilities: Some(vec![vec![format!("rate={}", egress_bps)]]),
                ..Default::default()
            });
        }

        Ok(device_requests)
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
                let container_port = port_config.port;
                let container_port_key = format!("{}/tcp", container_port);
                exposed_ports.insert(container_port_key.clone(), HashMap::new());

                // Handle port mapping
                if let Some(target_port) = port_config.target_port {
                    let host_binding = PortBinding {
                        host_ip: Some(String::from("0.0.0.0")),
                        host_port: Some(target_port.to_string()),
                    };
                    port_bindings.insert(container_port_key, Some(vec![host_binding]));
                }

                assigned_port_metadata.push(ContainerPortMetadata {
                    port: container_port,
                    target_port: port_config.target_port,
                    node_port: port_config.node_port,
                });
            }
        }

        Ok((port_bindings, exposed_ports, assigned_port_metadata))
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn get_image_digest(&self, image: &str) -> Result<String> {
        let inspect = self.client.inspect_image(image).await?;

        // Get the image digest
        if let Some(id) = inspect.id {
            Ok(id)
        } else {
            Err(anyhow!("Failed to get image digest"))
        }
    }

    async fn check_image_updates(
        &self,
        _service_name: &str,
        containers: &[Container],
        current_hashes: &HashMap<String, String>,
    ) -> Result<HashMap<String, bool>> {
        let mut updates = HashMap::new();

        for container in containers {
            let current_hash = current_hashes.get(&container.name);
            let new_hash = self.get_image_digest(&container.image).await?;

            updates.insert(
                container.name.clone(),
                current_hash.map_or(true, |h| h != &new_hash),
            );
        }

        Ok(updates)
    }

    async fn remove_pod_network(&self, network_name: &str, service_name: &str) -> Result<()> {
        if network_name == "bridge" {
            return Ok(());
        }

        if network_name.contains("__") {
            // Always remove auto-generated networks
            self.client.remove_network(network_name).await?;
        } else {
            // For user-defined networks, check if no more services are using it
            if self.untrack_network_usage(network_name, service_name).await {
                self.client.remove_network(network_name).await?;
            }
        }
        Ok(())
    }

    async fn create_pod_network(&self, service_name: &str, uuid: &str) -> Result<String> {
        let network_name = format!("{}__{}", service_name, uuid);

        // Check if network exists and remove if it does
        if let Ok(networks) = self.client.list_networks::<String>(None).await {
            if networks
                .iter()
                .any(|n| n.name == Some(network_name.clone()))
            {
                self.client.remove_network(&network_name).await?;
            }
        }

        // Create network
        self.client
            .create_network(CreateNetworkOptions {
                name: network_name.clone(),
                driver: "bridge".to_string(),
                ..Default::default()
            })
            .await?;

        Ok(network_name)
    }

    async fn start_containers(
        &self,
        service_name: &str,
        pod_number: u8,
        containers: &Vec<Container>,
        service_config: &ServiceConfig,
    ) -> Result<Vec<(String, String, Vec<ContainerPortMetadata>)>> {
        const MAX_RETRIES: u32 = 3;
        let mut retry_count = 0;

        while retry_count < MAX_RETRIES {
            match self
                .attempt_start_containers(service_name, pod_number, containers, &service_config)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    retry_count += 1;
                    if retry_count == MAX_RETRIES {
                        return Err(e);
                    }
                    slog::warn!(slog_scope::logger(), "Container creation failed, retrying";
                        "service" => service_name,
                        "retry" => retry_count,
                        "error" => e.to_string()
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }

        Err(anyhow!("Max retries exceeded"))
    }

    async fn attempt_start_containers(
        &self,
        service_name: &str,
        pod_number: u8,
        containers: &Vec<Container>,
        service_config: &ServiceConfig,
    ) -> Result<Vec<(String, String, Vec<ContainerPortMetadata>)>> {
        let uuid = Uuid::new_v4();
        // Setup network based on container count
        let network_name = self
            .setup_pod_network(
                service_name,
                &uuid.to_string(),
                containers.len(),
                service_config,
            )
            .await?;

        let mut started_containers = Vec::new();
        let mut containers_to_cleanup = Vec::new();
        let mut pod_creation_failed = false;
        let mut temp_dirs = Vec::new();

        for container in containers {
            let container_name =
                container.generate_runtime_name(service_name, pod_number, &uuid.to_string())?;

            // Setup volume mounts first and keep temp_dir alive
            let (temp_dir, mounts) = self
                .setup_volume_mounts(container, &container_name, &service_config)
                .await?;
            if let Some(dir) = temp_dir {
                temp_dirs.push(dir);
            }

            let (port_bindings, exposed_ports, assigned_port_metadata) =
                self.prepare_port_configuration(container).await?;

            // Get container-specific limits, falling back to service-level limits
            let memory_limit = container
                .memory_limit
                .as_ref()
                .map(parse_memory_limit)
                .transpose()?
                .or_else(|| {
                    service_config
                        .memory_limit
                        .as_ref()
                        .map(parse_memory_limit)
                        .transpose()
                        .ok()
                        .flatten()
                })
                .unwrap_or(0);

            let cpu_limit = container
                .cpu_limit
                .as_ref()
                .map(parse_cpu_limit)
                .transpose()?
                .or_else(|| {
                    service_config
                        .cpu_limit
                        .as_ref()
                        .map(parse_cpu_limit)
                        .transpose()
                        .ok()
                        .flatten()
                })
                .unwrap_or(0);

            let mut host_config = HostConfig {
                port_bindings: Some(port_bindings),
                memory: Some(memory_limit.try_into().unwrap()),
                nano_cpus: Some(cpu_limit as i64),
                network_mode: network_name.clone().or(Some("bridge".to_string())),
                ..Default::default()
            };

            if !mounts.is_empty() {
                host_config.mounts = Some(mounts);
            }

            // Apply network limits if specified
            if let Some(network_limit) = &container.network_limit {
                let device_requests = self.prepare_network_limits(network_limit)?;
                if !device_requests.is_empty() {
                    host_config.device_requests = Some(device_requests);
                }
            }

            let mut config = Config {
                image: Some(container.image.clone()),
                host_config: Some(host_config),
                exposed_ports: Some(exposed_ports),
                // this helps avoid a collision if networks are being shared, as service_name is unique
                hostname: Some(format!("{}-{}", service_name, container.name)),
                ..Default::default()
            };

            if let Some(cmd) = &container.command {
                config.cmd = Some(cmd.clone());
            }

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
                            if let Ok(container_data) =
                                self.client.inspect_container(&container_name, None).await
                            {
                                if let Some(network_settings) = container_data.network_settings {
                                    if let Some(networks) = network_settings.networks {
                                        // Handle Option<String> for network_name
                                        let network_key =
                                            network_name.as_deref().unwrap_or("bridge");
                                        if let Some(network) = networks.get(network_key) {
                                            if let Some(ip) = &network.ip_address {
                                                containers_to_cleanup
                                                    .push((container_name.clone(), ip.clone()));
                                                started_containers.push((
                                                    container_name,
                                                    ip.clone(),
                                                    assigned_port_metadata,
                                                ));
                                                continue;
                                            }
                                        }
                                    }
                                }
                                pod_creation_failed = true;
                            }
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

        if pod_creation_failed {
            for (container_name, _) in containers_to_cleanup {
                if let Err(e) = self.stop_container(&container_name).await {
                    slog::error!(slog_scope::logger(), "Failed to cleanup container";
                        "service" => service_name,
                        "container" => &container_name,
                        "error" => e.to_string()
                    );
                }
            }

            // Only remove custom network for multi-container pods
            if let Some(network_name) = network_name {
                self.remove_pod_network(&network_name, service_name).await?;
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
            .ok_or_else(|| anyhow!("No stats available for container {}", name))??;

        let container_data = self.client.inspect_container(name, None).await?;

        let mut ip_address = String::from("");

        //  Extract port mappings from container data
        let mut port_mappings = HashMap::new();
        if let Some(network_settings) = container_data.network_settings {
            ip_address = if let Some(networks) = network_settings.networks {
                networks
                    .values()
                    .next()
                    .and_then(|network| network.ip_address.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };

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

        let mut container_stats =
            update_container_stats(service_name, name, stats.clone(), nano_cpus).await;
        container_stats.ip_address = ip_address;
        container_stats.port_mappings = port_mappings;

        Ok(container_stats)
    }

    async fn list_containers(&self, service_name: Option<&str>) -> Result<Vec<ContainerInfo>> {
        let mut filters = HashMap::new();

        if let Some(service_name) = service_name {
            filters.insert("name".to_string(), vec![format!("{}__", service_name)]);
        }

        let containers = self
            .client
            .list_containers(Some(bollard::container::ListContainersOptions {
                all: false, // Change to only get running containers
                filters,
                ..Default::default()
            }))
            .await?;

        slog::debug!(slog_scope::logger(), "Found containers";
            "service" => service_name,
            "count" => containers.len(),
            // "containers" => ?containers
        );

        Ok(containers
            .into_iter()
            .filter(|c| c.state.as_deref() == Some("running"))
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
                    .and_then(|p| p.public_port)
                    .unwrap_or(0),
            })
            .collect())
    }
}
