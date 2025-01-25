pub mod docker;

use dashmap::DashMap;
use std::{collections::HashSet, sync::OnceLock};

// Track which services are using each network
pub static NETWORK_USAGE: OnceLock<DashMap<String, HashSet<String>>> = OnceLock::new();
