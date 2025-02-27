// src/config/utils.rs
use std::path::Path;

use anyhow::{anyhow, Result};
use uuid::Uuid;

use super::{ServiceConfig, CONFIG_STORE};

#[derive(Debug)]
pub struct ContainerNameParts {
    pub service_name: String,
    pub pod_number: u8,
    pub container_name: String,
    pub uuid: Uuid,
}

pub fn parse_container_name(container_name: &str) -> Result<ContainerNameParts> {
    let parts: Vec<&str> = container_name.split("__").collect();

    if parts.len() != 4 {
        return Err(anyhow!(
            "Container name does not match pattern 'service__pod-number__container-name__uuid': {}",
            container_name
        ));
    }

    let pod_number = parts[1].parse::<u8>().map_err(|e| {
        anyhow!(
            "Invalid pod number in container name '{}': {}",
            container_name,
            e
        )
    })?;

    let uuid = Uuid::parse_str(parts[3])
        .map_err(|e| anyhow!("Invalid UUID in container name '{}': {}", container_name, e))?;

    Ok(ContainerNameParts {
        service_name: parts[0].to_string(),
        pod_number,
        container_name: parts[2].to_string(),
        uuid,
    })
}

// Helper functions to access configs
pub async fn get_config_by_path(path: &str) -> Option<ServiceConfig> {
    if let Some(store) = CONFIG_STORE.get() {
        let store = store.read().await;
        store.get(path).map(|(_, config)| config.clone())
    } else {
        None
    }
}

pub async fn get_config_by_service(service_name: &str) -> Option<ServiceConfig> {
    if let Some(store) = CONFIG_STORE.get() {
        let store = store.read().await;
        store
            .values()
            .find(|(_, config)| config.name == service_name)
            .map(|(_, config)| config.clone())
    } else {
        None
    }
}

pub fn get_relative_config_path(full_path: &Path, config_dir: &Path) -> Option<String> {
    let config_dir_str = config_dir.to_str()?;
    let full_path_str = full_path.to_str()?;

    // Find the position of "configs/" in the full path
    if let Some(pos) = full_path_str.find(config_dir_str) {
        // Extract everything from "configs/" onwards
        let rel_path = &full_path_str[pos..];
        return Some(rel_path.to_string());
    }
    None
}
