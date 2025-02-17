// src/container/volumes.rs
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

pub static VOLUME_STORE: OnceLock<Arc<RwLock<FxHashMap<String, VolumeMetadata>>>> = OnceLock::new();
pub static VOLUME_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VolumeMetadata {
    pub name: String,
    pub id: Uuid,
    pub created_at: SystemTime,
    pub path: PathBuf,
    pub used_by: Vec<String>, // Container IDs using this volume
    pub labels: Option<HashMap<String, String>>,
    pub size: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NamedVolume {
    pub name: String,
    pub labels: Option<HashMap<String, String>>,
    pub driver_opts: Option<HashMap<String, String>>,
}

// Update VolumeData to support named volumes
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VolumeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_volume: Option<NamedVolume>,
}

use anyhow::{anyhow, Result};
use std::path::Path;
use tokio::fs;

pub async fn initialize_volume_store(volume_path: &Path) -> Result<()> {
    let store = Arc::new(RwLock::new(FxHashMap::default()));
    VOLUME_STORE.set(store).unwrap();
    VOLUME_PATH.get_or_init(|| volume_path.to_path_buf());

    fs::create_dir_all(volume_path).await?;

    // Load existing volumes
    let mut entries = fs::read_dir(volume_path).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let volume_name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();

            // Read metadata file if it exists
            let metadata_path = path.join("metadata.json");
            if metadata_path.exists() {
                let metadata = fs::read_to_string(&metadata_path).await?;
                let volume_metadata: VolumeMetadata = serde_json::from_str(&metadata)?;
                let mut store = VOLUME_STORE.get().unwrap().write().await;
                store.insert(volume_name, volume_metadata);
            }
        }
    }
    Ok(())
}

pub async fn create_named_volume(
    name: &str,
    labels: Option<HashMap<String, String>>,
) -> Result<VolumeMetadata> {
    let volume_store = VOLUME_STORE.get().expect("Volume store not initialized");
    let volume_path = VOLUME_PATH.get().expect("Volume path not initialized");

    // Check existence with read lock first
    {
        let store = volume_store.read().await;
        if store.contains_key(name) {
            return Err(anyhow!("Volume {} already exists", name));
        }
    }
    let volume_id = Uuid::new_v4();
    let volume_dir = volume_path.join(name);
    fs::create_dir_all(&volume_dir).await?;

    let metadata = VolumeMetadata {
        name: name.to_string(),
        id: volume_id,
        created_at: SystemTime::now(),
        path: volume_dir.clone(),
        used_by: Vec::new(),
        labels,
        size: None,
    };

    // Save metadata
    let metadata_path = volume_dir.join("metadata.json");
    fs::write(&metadata_path, serde_json::to_string(&metadata)?).await?;

    // Insert with write lock
    {
        let mut store = volume_store.write().await;
        store.insert(name.to_string(), metadata.clone());
    }

    Ok(metadata)
}

pub async fn remove_named_volume(name: &str, force: bool) -> Result<()> {
    let volume_store = VOLUME_STORE.get().expect("Volume store not initialized");

    // Check volume with read lock first
    let should_remove = {
        let store = volume_store.read().await;
        match store.get(name) {
            Some(metadata) => {
                if !metadata.used_by.is_empty() && !force {
                    return Err(anyhow!("Volume {} is still in use", name));
                }
                Some(metadata.path.clone())
            }
            None => None,
        }
    };

    if let Some(path) = should_remove {
        fs::remove_dir_all(&path).await?;
        // Remove from store with write lock
        let mut store = volume_store.write().await;
        store.remove(name);
    }
    Ok(())
}

pub async fn attach_volume(name: &str, container_id: &str) -> Result<()> {
    let volume_store = VOLUME_STORE.get().expect("Volume store not initialized");
    let container_id = container_id.to_string();

    // Acquire write lock once for all operations
    let mut store = volume_store.write().await;

    if let Some(metadata) = store.get_mut(name) {
        if !metadata.used_by.contains(&container_id) {
            metadata.used_by.push(container_id);

            // Update metadata file
            let metadata_path = metadata.path.join("metadata.json");
            fs::write(&metadata_path, serde_json::to_string(&metadata)?).await?;
        }
        Ok(())
    } else {
        Err(anyhow!("Volume {} not found", name))
    }
}

pub async fn detach_volume(name: &str, container_id: &str) -> Result<()> {
    let volume_store = VOLUME_STORE.get().expect("Volume store not initialized");

    // Acquire write lock once for all operations
    let mut store = volume_store.write().await;

    if let Some(metadata) = store.get_mut(name) {
        metadata.used_by.retain(|id| id != container_id);

        // Update metadata file
        let metadata_path = metadata.path.join("metadata.json");
        fs::write(&metadata_path, serde_json::to_string(&metadata)?).await?;
        Ok(())
    } else {
        Err(anyhow!("Volume {} not found", name))
    }
}
