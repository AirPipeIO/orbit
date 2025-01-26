// src/config/validate.rs
use anyhow::Result;

use std::collections::HashSet;
use thiserror::Error;

use super::{ServiceConfig, CONFIG_STORE};

#[derive(Error, Debug)]
pub enum ConfigValidationError {
    #[error("Duplicate service name '{0}' found")]
    DuplicateServiceName(String),
    #[error("Duplicate container name '{0}' found in service '{1}'")]
    DuplicateContainerName(String, String),
    #[error("Invalid service name '{0}': {1}")]
    InvalidServiceName(String, String),
    #[error("Invalid container name '{0}': {1}")]
    InvalidContainerName(String, String),
}

#[derive(Error, Debug)]
pub enum PortValidationError {
    #[error("Duplicate {port_type} port {port} found in service '{service}'")]
    DuplicatePortInService {
        port_type: String,
        port: u16,
        service: String,
    },
    #[error("{port_type} port {port} in service '{service1}' conflicts with service '{service2}'")]
    PortConflictBetweenServices {
        port_type: String,
        port: u16,
        service1: String,
        service2: String,
    },
}

// Validate ports within a single service config
pub fn validate_service_ports(config: &ServiceConfig) -> Result<(), PortValidationError> {
    let mut target_ports = HashSet::new();
    let mut node_ports = HashSet::new();

    for container in &config.spec.containers {
        if let Some(ports) = &container.ports {
            for port_config in ports {
                // Check target_ports against both target and node ports
                if let Some(target_port) = port_config.target_port {
                    if !target_ports.insert(target_port) || node_ports.contains(&target_port) {
                        return Err(PortValidationError::DuplicatePortInService {
                            port_type: "target".to_string(),
                            port: target_port,
                            service: config.name.clone(),
                        });
                    }
                }

                // Check node_ports against both node and target ports
                if let Some(node_port) = port_config.node_port {
                    if !node_ports.insert(node_port) || target_ports.contains(&node_port) {
                        return Err(PortValidationError::DuplicatePortInService {
                            port_type: "node".to_string(),
                            port: node_port,
                            service: config.name.clone(),
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

// Check for port conflicts between services
pub fn check_port_conflicts(
    new_config: &ServiceConfig,
    exclude_service: Option<&str>,
) -> Result<(), PortValidationError> {
    let config_store = CONFIG_STORE.get().expect("Config store not initialized");

    // Collect ports from new config
    let mut new_target_ports = HashSet::new();
    let mut new_node_ports = HashSet::new();

    for container in &new_config.spec.containers {
        if let Some(ports) = &container.ports {
            for port_config in ports {
                if let Some(target_port) = port_config.target_port {
                    new_target_ports.insert(target_port);
                }
                if let Some(node_port) = port_config.node_port {
                    new_node_ports.insert(node_port);
                }
            }
        }
    }

    // Check against all existing services
    for entry in config_store.iter() {
        let existing_config = &entry.value().1;

        // Skip if this is the service we're updating
        if let Some(exclude) = exclude_service {
            if existing_config.name == exclude {
                continue;
            }
        }

        // Skip comparing against self
        if existing_config.name == new_config.name {
            continue;
        }

        for container in &existing_config.spec.containers {
            if let Some(ports) = &container.ports {
                for port_config in ports {
                    // Check for conflicts between target_ports and existing node_ports, and vice versa
                    if let Some(target_port) = port_config.target_port {
                        if new_target_ports.contains(&target_port)
                            || new_node_ports.contains(&target_port)
                        {
                            return Err(PortValidationError::PortConflictBetweenServices {
                                port_type: "target".to_string(),
                                port: target_port,
                                service1: new_config.name.clone(),
                                service2: existing_config.name.clone(),
                            });
                        }
                    }

                    if let Some(node_port) = port_config.node_port {
                        if new_node_ports.contains(&node_port)
                            || new_target_ports.contains(&node_port)
                        {
                            return Err(PortValidationError::PortConflictBetweenServices {
                                port_type: "node".to_string(),
                                port: node_port,
                                service1: new_config.name.clone(),
                                service2: existing_config.name.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// Add validation functions
pub fn validate_service_name(name: &str) -> Result<(), ConfigValidationError> {
    // RFC 1123 DNS label validation
    let name_regex = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,61}[a-z0-9]$").unwrap();
    if !name_regex.is_match(name) {
        return Err(ConfigValidationError::InvalidServiceName(
            name.to_string(),
            "Service name must be a valid DNS label: lowercase alphanumeric characters or '-', starting and ending with alphanumeric".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_container_name(name: &str) -> Result<(), ConfigValidationError> {
    // Similar validation for container names
    let name_regex = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,61}[a-z0-9]$").unwrap();
    if !name_regex.is_match(name) {
        return Err(ConfigValidationError::InvalidContainerName(
            name.to_string(),
            "Container name must be a valid DNS label: lowercase alphanumeric characters or '-', starting and ending with alphanumeric".to_string(),
        ));
    }
    Ok(())
}

// Add this function to check for duplicate service names
pub fn check_service_name_uniqueness(
    config: &ServiceConfig,
    exclude_service: Option<&str>,
) -> Result<(), ConfigValidationError> {
    let config_store = CONFIG_STORE.get().expect("Config store not initialized");

    // Check if any existing config has the same name, excluding the service being updated
    for existing_config in config_store.iter() {
        if existing_config.value().1.name == config.name {
            // Skip if this is the service we're updating
            if let Some(exclude) = exclude_service {
                println!("\n\n{:?} : {:?} \n\n", exclude, config.name);
                if exclude == config.name {
                    continue;
                }
            }
            return Err(ConfigValidationError::DuplicateServiceName(
                config.name.clone(),
            ));
        }
    }

    Ok(())
}

// Add this function to check for duplicate container names within a service
pub fn check_container_name_uniqueness(
    config: &ServiceConfig,
) -> Result<(), ConfigValidationError> {
    let mut container_names = HashSet::new();

    for container in &config.spec.containers {
        if !container_names.insert(&container.name) {
            return Err(ConfigValidationError::DuplicateContainerName(
                container.name.clone(),
                config.name.clone(),
            ));
        }

        // Also validate the container name format
        validate_container_name(&container.name)?;
    }

    Ok(())
}
