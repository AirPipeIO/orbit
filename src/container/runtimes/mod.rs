// src/container/runtimes/mod.rs
pub mod docker;

use rustc_hash::FxHashMap;
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};
use tokio::sync::RwLock;

// Track which services are using each network
pub static NETWORK_USAGE: OnceLock<Arc<RwLock<FxHashMap<String, HashSet<String>>>>> =
    OnceLock::new();
