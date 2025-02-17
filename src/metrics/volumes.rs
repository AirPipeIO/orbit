// src/metrics/volumes.rs
use std::time::Duration;

use anyhow::{anyhow, Result};
use walkdir::WalkDir;

use crate::container::volumes::VOLUME_STORE;

use super::{VOLUME_CONTAINER_COUNT, VOLUME_TOTAL_COUNT, VOLUME_USAGE_BYTES};

async fn update_volume_size(name: &str) -> Result<u64> {
    let volume_store = VOLUME_STORE.get().expect("Volume store not initialized");

    // First get a read lock to check if the volume exists and get its path
    let volume_path = {
        let store = volume_store.read().await;
        match store.get(name) {
            Some(metadata) => metadata.path.clone(),
            None => return Err(anyhow!("Volume {} not found", name)),
        }
    };

    // Calculate size without holding any locks
    let size = WalkDir::new(&volume_path)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum();

    // Now get a write lock to update the metadata
    {
        let mut store = volume_store.write().await;
        if let Some(metadata) = store.get_mut(name) {
            metadata.size = Some(size);

            // Update metrics
            if let Some(usage_gauge) = VOLUME_USAGE_BYTES.get() {
                usage_gauge.with_label_values(&[name]).set(size as f64);
            }

            if let Some(container_gauge) = VOLUME_CONTAINER_COUNT.get() {
                container_gauge
                    .with_label_values(&[name])
                    .set(metadata.used_by.len() as f64);
            }

            // Update metadata file
            let metadata_path = metadata.path.join("metadata.json");
            tokio::fs::write(&metadata_path, serde_json::to_string(&metadata)?).await?;
        }
    }

    Ok(size)
}

pub async fn start_volume_metrics_task() {
    let mut interval = tokio::time::interval(Duration::from_secs(300)); // 5 minutes

    tokio::spawn(async move {
        loop {
            interval.tick().await;

            let volume_store = VOLUME_STORE.get().expect("Volume store not initialized");

            // First get all volume names with a read lock
            let volume_names = {
                let store = volume_store.read().await;
                store.keys().cloned().collect::<Vec<String>>()
            };

            // Update total volume count metric
            if let Some(total_volumes) = VOLUME_TOTAL_COUNT.get() {
                total_volumes.set(volume_names.len() as i64);
            }

            // Process each volume without holding the main lock
            for name in volume_names {
                if let Err(e) = update_volume_size(&name).await {
                    slog::error!(slog_scope::logger(), "Failed to update volume size";
                        "volume" => name,
                        "error" => e.to_string()
                    );
                }
            }
        }
    });
}
