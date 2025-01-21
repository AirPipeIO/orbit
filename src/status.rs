// status.rs
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use crate::{
    config::get_config_by_service,
    container::{ContainerStats, InstanceMetadata, INSTANCE_STORE},
    proxy::SERVER_BACKENDS,
};
use anyhow::Result;
use axum::Json;
use dashmap::DashMap;
use serde::Serialize;
use tokio::time::timeout;
use uuid::Uuid;

// status response
#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    service_name: String,
    service_url: String,
    containers: Vec<ContainerInfo>,
}

#[derive(Debug, Serialize)]
pub struct ContainerInfo {
    uuid: Uuid,
    exposed_port: u16,
    status: String,
    cpu_percentage: Option<f64>,
    cpu_percentage_relative: Option<f64>,
    memory_usage: Option<u64>,
    memory_limit: Option<u64>,
}

// Global cache for instance store data
pub static INSTANCE_STORE_CACHE: OnceLock<Arc<DashMap<String, HashMap<Uuid, InstanceMetadata>>>> =
    OnceLock::new();

// cache for container stats
pub static CONTAINER_STATS_CACHE: OnceLock<Arc<DashMap<String, ContainerStats>>> = OnceLock::new();

pub fn update_instance_store_cache() {
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised");
    let instance_cache = INSTANCE_STORE_CACHE.get_or_init(|| Arc::new(DashMap::new()));

    // Update instance cache
    instance_cache.clear();
    for entry in instance_store.iter() {
        instance_cache.insert(entry.key().clone(), entry.value().clone());
    }
}

pub fn initialize_background_cache_update() {
    let log = slog_scope::logger();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            if let Err(e) = update_cache_with_timeout().await {
                slog::warn!(log, "Cache update failed"; "error" => e.to_string());
            }
        }
    });
}

// Replace the existing update_instance_store_cache with this version
async fn update_cache_with_timeout() -> Result<()> {
    // Add timeout to prevent blocking
    timeout(Duration::from_secs(5), async {
        let instance_store = INSTANCE_STORE
            .get()
            .expect("Instance store not initialised");
        let instance_cache = INSTANCE_STORE_CACHE.get_or_init(|| Arc::new(DashMap::new()));

        // Clear existing instance cache
        instance_cache.clear();

        // Update instance cache in batches
        for entry in instance_store.iter() {
            instance_cache.insert(entry.key().clone(), entry.value().clone());
            tokio::task::yield_now().await; // Allow other tasks to run
        }
    })
    .await?;

    Ok(())
}

// Modify the get_status handler to use cached data
pub async fn get_status() -> Json<Vec<ServiceStatus>> {
    let instance_cache = INSTANCE_STORE_CACHE
        .get()
        .expect("Instance cache not initialized");
    let server_backends = SERVER_BACKENDS
        .get()
        .expect("Server backends not initialized");
    let stats_cache = CONTAINER_STATS_CACHE
        .get()
        .expect("Stats cache not initialized");

    let mut services = Vec::new();

    for entry in instance_cache.iter() {
        let service_name = entry.key();
        let instances = entry.value();

        let service_config = get_config_by_service(service_name);

        if let Some(config) = service_config {
            let containers: Vec<ContainerInfo> = instances
                .iter()
                .map(|(uuid, metadata)| {
                    let container_name = format!("{}__{}", service_name, uuid);
                    let container_addr = format!("127.0.0.1:{}", metadata.exposed_port);

                    let backends = server_backends.get(service_name);
                    let status = if backends.iter().any(|b| {
                        b.iter()
                            .any(|backend| backend.addr.to_string() == container_addr)
                    }) {
                        "running".to_string()
                    } else {
                        "unknown".to_string()
                    };

                    // Get cached stats
                    let stats = stats_cache.get(&container_name);

                    ContainerInfo {
                        uuid: *uuid,
                        exposed_port: metadata.exposed_port,
                        status,
                        cpu_percentage: stats.as_ref().map(|s| s.cpu_percentage),
                        cpu_percentage_relative: stats.as_ref().map(|s| s.cpu_percentage_relative),
                        memory_usage: stats.as_ref().map(|s| s.memory_usage),
                        memory_limit: stats.as_ref().map(|s| s.memory_limit),
                    }
                })
                .collect();

            services.push(ServiceStatus {
                service_name: service_name.clone(),
                service_url: format!("http://0.0.0.0:{}", config.exposed_port),
                containers,
            });
        }
    }

    Json(services)
}
