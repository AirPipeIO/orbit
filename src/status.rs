// status.rs
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use crate::{
    config::{ServiceConfig, CONFIG_STORE},
    container::{InstanceMetadata, INSTANCE_STORE},
    proxy::SERVER_BACKENDS,
};
use axum::{routing::get, Json, Router};
use dashmap::DashMap;
use serde::Serialize;
use uuid::Uuid;

// status response
#[derive(Debug, Serialize)]
struct ServiceStatus {
    service_name: String,
    service_url: String,
    containers: Vec<ContainerInfo>,
}

#[derive(Debug, Serialize)]
struct ContainerInfo {
    uuid: Uuid,
    exposed_port: u16,
    status: String,
}

// Global cache for instance store data
pub static INSTANCE_STORE_CACHE: OnceLock<Arc<DashMap<String, HashMap<Uuid, InstanceMetadata>>>> =
    OnceLock::new();

pub fn update_instance_store_cache() {
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised");
    let cache = INSTANCE_STORE_CACHE.get_or_init(|| Arc::new(DashMap::new()));

    cache.clear();
    for entry in instance_store.iter() {
        cache.insert(entry.key().clone(), entry.value().clone());
    }
}

// Start the status server
pub async fn server_start() {
    let log: slog::Logger = slog_scope::logger();

    let app = Router::new().route("/status", get(get_status));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4112").await.unwrap();
    slog::info!(log, "Status server running on http://0.0.0.0:4112");

    axum::serve(listener, app).await.unwrap();
}

// Handle the `/status` endpoint
async fn get_status() -> Json<Vec<ServiceStatus>> {
    let instance_store = INSTANCE_STORE_CACHE
        .get()
        .clone()
        .expect("Instance store not initialized");
    let config_store = CONFIG_STORE
        .get()
        .clone()
        .expect("Config store not initialized");
    let server_backends = SERVER_BACKENDS
        .get()
        .expect("Server backends not initialized");

    // Clone data outside of locks
    let instance_data: Vec<(String, HashMap<Uuid, InstanceMetadata>)> = instance_store
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();

    let config_data: Vec<ServiceConfig> = config_store
        .iter()
        .map(|entry| entry.value().clone())
        .collect();

    let mut services = Vec::new();

    for (service_name, instances) in instance_data {
        let backends = server_backends
            .get(&service_name)
            .map(|b| {
                b.iter()
                    .map(|backend| backend.addr.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let containers = instances
            .iter()
            .map(|(uuid, metadata)| {
                let container_addr = format!("127.0.0.1:{}", metadata.exposed_port);
                let container_socket_addr: Option<std::net::SocketAddr> =
                    container_addr.parse().ok();

                let status = if let Some(socket_addr) = container_socket_addr {
                    if backends
                        .iter()
                        .any(|backend| backend.to_string() == socket_addr.to_string())
                    {
                        "running"
                    } else {
                        "unknown"
                    }
                } else {
                    "unknown"
                };

                ContainerInfo {
                    uuid: *uuid,
                    exposed_port: metadata.exposed_port,
                    status: status.to_string(),
                }
            })
            .filter(|a| a.status != "unknown")
            .collect();

        let service_config = config_data
            .iter()
            .find(|config| config.name == service_name)
            .expect("ServiceConfig not found");

        services.push(ServiceStatus {
            service_name: service_name.clone(),
            service_url: format!("http://0.0.0.0:{}", service_config.exposed_port),
            containers,
        });
    }

    Json(services)
}
