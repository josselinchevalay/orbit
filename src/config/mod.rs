// src/config/mod.rs
pub mod utils;
pub mod validate;
use rustc_hash::FxHashMap;
pub use utils::*;

use crate::container::health::{HealthState, CONTAINER_HEALTH};
use crate::container::scaling::manager::ScalingPolicy;
use crate::container::volumes::VolumeData;
use crate::container::{rolling_update, Container, IMAGE_CHECK_TASKS};
use anyhow::{anyhow, Result};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::sync::Arc;
use std::{
    collections::HashMap,
    path::PathBuf,
    path::Path,
    sync::OnceLock,
    time::{Duration, SystemTime},
};
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;
use validate::{
    check_container_name_uniqueness, check_port_conflicts, check_service_name_uniqueness,
    validate_service_name, validate_service_ports,
};
use validator::Validate;

use crate::{
    container::{
        self, clean_up, manage, remove_container_stats, scaling::auto_scale, ContainerInfo,
        ContainerMetadata, ContainerPortMetadata, ContainerStats, InstanceMetadata, INSTANCE_STORE,
        RUNTIME, SCALING_TASKS,
    },
    proxy::{self, SERVER_BACKENDS},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RollingUpdateConfig {
    #[serde(default = "default_max_unavailable")]
    pub max_unavailable: u8,
    #[serde(default = "default_max_surge")]
    pub max_surge: u8,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

fn default_max_unavailable() -> u8 {
    1
}
fn default_max_surge() -> u8 {
    1
}

impl Default for RollingUpdateConfig {
    fn default() -> Self {
        Self {
            max_unavailable: default_max_unavailable(),
            max_surge: default_max_surge(),
            timeout: Duration::from_secs(300), // 5 minute default timeout
        }
    }
}

// Add new validation error types

#[derive(Clone, Debug)]
pub enum ScaleMessage {
    ConfigUpdate, // Added version number
    Resume,       // Resume with version to ensure matching
    RollingUpdate,
    RollingUpdateComplete,
}

// pull policy value
#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum PullPolicyValue {
    Always,
    Never,
}

pub static CONFIG_UPDATES: OnceLock<mpsc::Sender<(String, ScaleMessage)>> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstanceCount {
    pub min: u8, // Minimum instances to keep running
    pub max: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoDelConfig {
    /// Target delay threshold in milliseconds
    #[serde(with = "humantime_serde")]
    pub target: Duration,

    /// Interval for checking delays in milliseconds
    #[serde(with = "humantime_serde")]
    pub interval: Duration,

    /// Number of consecutive intervals above target before scaling
    #[serde(default = "default_consecutive_intervals")]
    pub consecutive_intervals: u32,

    /// Maximum instances to scale up at once
    #[serde(default = "default_max_scale_step")]
    pub max_scale_step: u32,

    /// Minimum time between scaling actions
    #[serde(with = "humantime_serde")]
    pub scale_cooldown: Duration,

    /// HTTP status code to return when overloaded
    #[serde(default)]
    pub overload_status_code: Option<u16>,
}

fn default_consecutive_intervals() -> u32 {
    3
}

fn default_max_scale_step() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
pub struct ServiceConfig {
    #[validate(length(max = 210))]
    pub name: String,
    pub network: Option<String>,
    pub spec: ServiceSpec,
    pub memory_limit: Option<Value>,
    pub pull_policy: Option<PullPolicyValue>,
    pub cpu_limit: Option<Value>,
    pub resource_thresholds: Option<ResourceThresholds>,
    pub instance_count: InstanceCount,
    #[serde(default = "default_instance_count")]
    pub adopt_orphans: bool,
    pub interval_seconds: Option<u64>,
    #[serde(with = "humantime_serde", default)]
    pub image_check_interval: Option<Duration>,
    pub rolling_update_config: Option<RollingUpdateConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<HashMap<String, VolumeData>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codel: Option<CoDelConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scaling_policy: Option<ScalingPolicy>,
}

fn default_instance_count() -> bool {
    false
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServiceSpec {
    pub containers: Vec<Container>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum PodMetricsStrategy {
    #[serde(rename = "max")]
    Maximum,
    #[serde(rename = "average")]
    Average,
}

impl Default for PodMetricsStrategy {
    fn default() -> Self {
        Self::Maximum
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResourceThresholds {
    pub cpu_percentage: Option<u8>,
    pub cpu_percentage_relative: Option<u8>,
    pub memory_percentage: Option<u8>,
    #[serde(default)]
    pub metrics_strategy: PodMetricsStrategy,
}

#[derive(Debug)]
pub struct PodStats {
    pub cpu_percentage: f64,
    pub cpu_percentage_relative: f64,
    pub memory_usage: u64,
    pub memory_limit: u64,
}

pub fn aggregate_pod_stats(
    container_stats: &[(Uuid, InstanceMetadata, ContainerStats)],
    strategy: &PodMetricsStrategy,
) -> PodStats {
    match strategy {
        PodMetricsStrategy::Maximum => {
            let mut max_stats = PodStats {
                cpu_percentage: 0.0,
                cpu_percentage_relative: 0.0,
                memory_usage: 0,
                memory_limit: 0,
            };

            for stats in container_stats {
                max_stats.cpu_percentage = max_stats.cpu_percentage.max(stats.2.cpu_percentage);
                max_stats.cpu_percentage_relative = max_stats
                    .cpu_percentage_relative
                    .max(stats.2.cpu_percentage_relative);
                max_stats.memory_usage = max_stats.memory_usage.max(stats.2.memory_usage);
                max_stats.memory_limit = max_stats.memory_limit.max(stats.2.memory_limit);
            }

            max_stats
        }
        PodMetricsStrategy::Average => {
            let count = container_stats.len() as f64;
            let sum_stats = container_stats.iter().fold(
                PodStats {
                    cpu_percentage: 0.0,
                    cpu_percentage_relative: 0.0,
                    memory_usage: 0,
                    memory_limit: 0,
                },
                |mut acc, stats| {
                    acc.cpu_percentage += stats.2.cpu_percentage;
                    acc.cpu_percentage_relative += stats.2.cpu_percentage_relative;
                    acc.memory_usage += stats.2.memory_usage;
                    acc.memory_limit += stats.2.memory_limit;
                    acc
                },
            );

            PodStats {
                cpu_percentage: sum_stats.cpu_percentage / count,
                cpu_percentage_relative: sum_stats.cpu_percentage_relative / count,
                memory_usage: sum_stats.memory_usage / count as u64,
                memory_limit: sum_stats.memory_limit / count as u64,
            }
        }
    }
}

pub static CONFIG_STORE: OnceLock<Arc<RwLock<FxHashMap<String, (PathBuf, ServiceConfig)>>>> =
    OnceLock::new();

pub async fn watch_directory(config_dir: PathBuf) -> notify::Result<()> {
    let log: slog::Logger = slog_scope::logger();

    let (tx, mut rx) = mpsc::channel(100);

    let tx_clone: mpsc::Sender<DebouncedEvent> = tx.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(100), // Adjust as needed
        None,
        move |res: DebounceEventResult| {
            let tx = tx_clone.clone();
            if let Ok(events) = res {
                for event in events {
                    if event.paths.iter().any(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .map_or(false, |ext| ext == "yml" || ext == "yaml")
                    }) && matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        let _ = tx.blocking_send(event);
                    }
                }
            }
        },
    )?;

    debouncer.watch(&config_dir, RecursiveMode::Recursive)?;
    slog::debug!(log, "watching directory"; "directory" => config_dir.to_str());

    while let Some(event) = rx.recv().await {
        process_event(event, &config_dir).await;
    }

    Ok(())
}

async fn process_event(event: DebouncedEvent, config_dir: &Path) {
    let config_store = CONFIG_STORE.get().unwrap();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    // Process the immediate event
    for path in event.paths.iter() {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                if path.exists() && path.is_file() {
                    let rel_config_path = get_relative_config_path(path, config_dir).unwrap();
                    // Check if there's an existing config for this path

                    // Check if it's a YAML file
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if ext != "yml" && ext != "yaml" {
                            slog::debug!(slog_scope::logger(), "Ignoring non-YAML file";
                                "path" => path.to_str(),
                                "extension" => ext
                            );
                            continue;
                        }
                    }

                    let existing_service = match event.kind {
                        EventKind::Modify(_) => {
                            let store = config_store.read().await;
                            store
                                .get(&rel_config_path)
                                .map(|(_, config)| config.name.clone())
                        }
                        _ => None,
                    };

                    match read_yaml_config(path, existing_service.as_deref()).await {
                        Ok(config) => {
                            let service_name = config.name.clone();

                            slog::info!(slog_scope::logger(), "Processing YAML config";
                                "service" => &service_name,
                                "path" => path.to_str()
                            );

                            // Store config with write lock
                            {
                                let mut store = config_store.write().await;
                                store.insert(rel_config_path, (path.to_path_buf(), config.clone()));
                            }

                            // Stop existing scaling task if it exists using write lock
                            {
                                let mut tasks = scaling_tasks.write().await;
                                if let Some(handle) = tasks.remove(&service_name) {
                                    handle.abort();
                                    slog::debug!(slog_scope::logger(), "Aborted existing scaling task";
                                        "service" => &service_name
                                    );
                                }
                            }

                            // Start containers and proxy
                            container::manage(&service_name, config.clone()).await;
                            proxy::run_proxy_for_service(service_name.clone(), config.clone())
                                .await;

                            let svc_name = service_name.clone();

                            // Create new scaling task
                            let handle = tokio::spawn(async move {
                                auto_scale(svc_name).await;
                            });

                            // Store new task handle with write lock
                            {
                                let mut tasks = scaling_tasks.write().await;
                                tasks.insert(service_name.clone(), handle);
                            }

                            slog::info!(slog_scope::logger(), "Service initialization complete";
                                "service" => &service_name
                            );
                        }
                        Err(e) => {
                            slog::error!(slog_scope::logger(), "Failed to parse YAML config";
                                "file" => path.to_str(),
                                "error" => e.to_string()
                            );
                        }
                    }
                }
            }
            EventKind::Remove(_) => {
                // Handle explicit removal events with read lock first
                let config_to_remove = {
                    let store = config_store.read().await;
                    store
                        .get(&path.display().to_string())
                        .map(|(_, config)| config.name.clone())
                };

                if let Some(service_name) = config_to_remove {
                    // Then use write lock to remove
                    {
                        let mut store = config_store.write().await;
                        store.remove(&path.display().to_string());
                    }

                    slog::info!(slog_scope::logger(), "Config file removed, cleaning up service";
                        "service" => &service_name,
                        "path" => path.to_str()
                    );

                    // Stop scaling task with write lock
                    {
                        let mut tasks = scaling_tasks.write().await;
                        if let Some(handle) = tasks.remove(&service_name) {
                            handle.abort();
                        }
                    }

                    tokio::spawn(async move {
                        stop_service(&service_name).await;
                        clean_up(&service_name).await;

                        slog::info!(slog_scope::logger(), "Service cleanup completed";
                            "service" => &service_name
                        );
                    });
                }
            }
            _ => {}
        }
    }

    // After processing the event, verify all tracked configs still exist
    let services_to_cleanup = {
        let store = config_store.read().await;
        store
            .iter()
            .filter_map(|(_path_str, (path, config))| {
                if !path.exists()
                    || !matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("yml") | Some("yaml")
                    )
                {
                    Some((path.clone(), config.name.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    for (path, service_name) in services_to_cleanup {
        slog::info!(slog_scope::logger(), "Config file no longer valid, cleaning up service";
            "service" => &service_name,
            "path" => path.to_str()
        );

        // Remove from config store with write lock
        {
            let mut store = config_store.write().await;
            store.remove(&path.display().to_string());
        }

        // Stop scaling task with write lock
        {
            let mut tasks = scaling_tasks.write().await;
            if let Some(handle) = tasks.remove(&service_name) {
                handle.abort();
            }
        }

        let service_name_clone = service_name.clone();
        tokio::spawn(async move {
            stop_service(&service_name_clone).await;
            clean_up(&service_name_clone).await;

            slog::info!(slog_scope::logger(), "Service cleanup completed";
                "service" => &service_name_clone
            );
        });
    }
}

pub async fn read_yaml_config(
    path: &PathBuf,
    exclude_service: Option<&str>,
) -> Result<ServiceConfig> {
    let log = slog_scope::logger();

    let path_str = path.to_str().unwrap();
    if path_str.ends_with(".yml") || path_str.ends_with(".yaml") {
        let contents = tokio::fs::read_to_string(path).await?;
        let config: ServiceConfig = serde_yaml::from_str(&contents)?;

        // Validate service name format
        validate_service_name(&config.name)?;

        // Check for duplicate service names (no exclusion for new configs)
        check_service_name_uniqueness(&config, exclude_service).await?;

        // Check for duplicate container names
        check_container_name_uniqueness(&config)?;

        // Validate ports within the service
        validate_service_ports(&config)?;

        // Check for conflicts with other services
        check_port_conflicts(&config, None).await?;

        // Debug log the parsed thresholds
        if let Some(thresholds) = &config.resource_thresholds {
            slog::debug!(log, "Parsed config thresholds";
                    "service" => &config.name,
                    "cpu_percentage" => thresholds.cpu_percentage,
                    "cpu_relative" => thresholds.cpu_percentage_relative,
                    "memory_percentage" => thresholds.memory_percentage);
        }

        return Ok(config);
    }

    Err(anyhow!("Not a yaml file {:?}", path))
}

pub async fn initialize_configs(config_dir: &PathBuf) -> Result<()> {
    let config_store = CONFIG_STORE.get().unwrap();
    let scaling_tasks = SCALING_TASKS.get().expect("Scaling tasks not initialized");
    let image_check_tasks = IMAGE_CHECK_TASKS
        .get()
        .expect("Image check tasks not initialized");
    let log = slog_scope::logger();

    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml")
            || path.extension().and_then(|ext| ext.to_str()) == Some("yml")
        {
            match read_yaml_config(&path, None).await {
                Ok(config) => {
                    slog::info!(log, "Initialising config";
                        "service" => &config.name,
                        "path" => path.display().to_string()
                    );

                    // Insert with write lock
                    {
                        let mut store = config_store.write().await;
                        store.insert(path.display().to_string(), (path.clone(), config.clone()));
                    }

                    // Handle orphaned containers based on the adopt_orphans flag
                    handle_orphans(&config).await?;

                    container::manage(&config.name, config.clone()).await;
                    proxy::run_proxy_for_service(config.name.to_string(), config.clone()).await;

                    let service_name: String = config.name.clone();

                    // Start auto-scaling task
                    // Update scaling task creation:
                    let service_name_clone = service_name.clone();
                    let handle = tokio::spawn(async move {
                        auto_scale(service_name_clone).await;
                    });

                    // Store the task handle with write lock
                    {
                        let mut tasks = scaling_tasks.write().await;
                        tasks.insert(service_name.clone(), handle);
                    }

                    let svc_name: String = config.name.clone();

                    let handle = tokio::spawn(async move {
                        if let Err(e) =
                            rolling_update::start_image_check_task(service_name.clone(), config)
                                .await
                        {
                            slog::error!(slog_scope::logger(), "Image check task failed";
                                "error" => e.to_string()
                            );
                        }
                    });

                    // Store the task handle with write lock
                    {
                        let mut tasks = image_check_tasks.write().await;
                        tasks.insert(svc_name.clone(), handle);
                    }
                }
                Err(e) => {
                    slog::error!(log, "Failed to load config";
                        "file" => path.to_str(),
                        "error" => e.to_string()
                    );
                }
            }
        }
    }

    Ok(())
}

pub async fn handle_orphans(config: &ServiceConfig) -> Result<()> {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();
    let service_name = &config.name;

    let orphaned_containers = runtime.list_containers(Some(service_name)).await?;
    if orphaned_containers.is_empty() {
        return Ok(());
    }

    let orphan_count = orphaned_containers.len();
    slog::info!(log, "Found orphaned containers";
        "service" => service_name,
        "count" => orphan_count
    );

    if config.adopt_orphans {
        let mut pod_containers: HashMap<Uuid, Vec<ContainerInfo>> = HashMap::new();

        for container in orphaned_containers {
            if let Ok(parts) = parse_container_name(&container.name) {
                pod_containers
                    .entry(parts.uuid)
                    .or_default()
                    .push(container);
            }
        }

        let required_container_count = config.spec.containers.len();
        let incomplete_pod_uuids: Vec<_> = pod_containers
            .iter()
            .filter(|(_, containers)| containers.len() != required_container_count)
            .map(|(uuid, _)| *uuid)
            .collect();

        for uuid in &incomplete_pod_uuids {
            if let Some(containers) = pod_containers.get(uuid) {
                let network_name = format!("{}__{}", service_name, uuid);

                for container in containers {
                    if let Err(e) = runtime.stop_container(&container.name).await {
                        slog::error!(log, "Failed to remove container from incomplete pod";
                            "service" => service_name,
                            "container" => &container.name,
                            "error" => e.to_string()
                        );
                    }
                }

                if let Err(e) = runtime
                    .remove_pod_network(&network_name, service_name)
                    .await
                {
                    slog::error!(log, "Failed to remove network";
                        "service" => service_name,
                        "network" => &network_name,
                        "error" => e.to_string()
                    );
                }
            }
        }

        for uuid in incomplete_pod_uuids {
            pod_containers.remove(&uuid);
        }

        // Update instance store with write lock
        {
            let mut store = instance_store.write().await;
            let instances = store
                .entry(service_name.to_string())
                .or_insert_with(FxHashMap::default);

            let mut adopted_count = 0;

            for (uuid, containers) in &pod_containers {
                let network_name = format!("{}__{}", service_name, uuid);
                let mut pod_metadata = Vec::new();

                for container in containers {
                    if let Ok(parts) = parse_container_name(&container.name) {
                        if let Some(container_config) = config
                            .spec
                            .containers
                            .iter()
                            .find(|c| c.name == parts.container_name)
                        {
                            if let Some(port_configs) = &container_config.ports {
                                if let Ok(container_data) =
                                    runtime.inspect_container(&container.name).await
                                {
                                    let port_metadata: Vec<ContainerPortMetadata> = port_configs
                                        .iter()
                                        .map(|p| ContainerPortMetadata {
                                            port: p.port,
                                            target_port: p.target_port,
                                            node_port: p.node_port,
                                        })
                                        .collect();

                                    pod_metadata.push(ContainerMetadata {
                                        name: container.name.clone(),
                                        network: network_name.clone(),
                                        ip_address: container_data.ip_address,
                                        ports: port_metadata,
                                        status: "adopted".to_string(),
                                    });
                                    adopted_count += 1;
                                } else {
                                    slog::error!(log, "Failed to inspect container";
                                        "service" => service_name,
                                        "container" => &container.name,
                                    );
                                }
                            }
                        }
                    }
                }

                if !pod_metadata.is_empty() {
                    let now = SystemTime::now();
                    let mut image_hashes = HashMap::new();

                    // Try to get image hashes for existing containers
                    for container in containers {
                        if let Ok(parts) = parse_container_name(&container.name) {
                            if let Some(container_config) = config
                                .spec
                                .containers
                                .iter()
                                .find(|c| c.name == parts.container_name)
                            {
                                if let Ok(hash) =
                                    runtime.get_image_digest(&container_config.image).await
                                {
                                    image_hashes.insert(container_config.name.clone(), hash);
                                }
                            }
                        }
                    }

                    instances.insert(
                        *uuid,
                        InstanceMetadata {
                            uuid: *uuid,
                            created_at: now,
                            network: network_name,
                            image_hash: image_hashes,
                            containers: pod_metadata,
                        },
                    );
                }
            }

            slog::info!(log, "Adopted orphaned containers";
                "service" => service_name,
                "adopted_pods" => pod_containers.len(),
                "adopted_containers" => adopted_count.to_string()
            );
        }
    } else {
        // Group containers by their network
        let mut network_containers: HashMap<String, Vec<String>> = HashMap::new();

        for container in &orphaned_containers {
            if let Ok(parts) = parse_container_name(&container.name) {
                let network_name = format!("{}__{}", service_name, parts.uuid);
                network_containers
                    .entry(network_name)
                    .or_default()
                    .push(container.name.clone());
            }
        }

        // Process each network and its containers
        for (network_name, containers) in network_containers {
            // First stop all containers in the network
            let mut stop_futures = Vec::new();
            for container_name in containers {
                let runtime = runtime.clone();
                let service_name = service_name.to_string();

                stop_futures.push(tokio::spawn(async move {
                    if let Err(e) = runtime.stop_container(&container_name).await {
                        slog::error!(slog_scope::logger(), "Failed to remove orphaned container";
                            "service" => %service_name,
                            "container" => %container_name,
                            "error" => %e
                        );
                        return Err(e);
                    }
                    Ok(())
                }));
            }

            // Wait for all containers to be stopped
            let _ = futures::future::join_all(stop_futures).await;

            // Add a small delay to ensure Docker has processed container removals
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Then try to remove the network
            if let Err(e) = runtime
                .remove_pod_network(&network_name, service_name)
                .await
            {
                slog::error!(slog_scope::logger(), "Failed to remove orphaned network";
                    "service" => %service_name,
                    "network" => %network_name,
                    "error" => %e
                );
            }
        }
    }

    Ok(())
}

// Update the stop_service function to ensure complete cleanup
pub async fn stop_service(service_name: &str) {
    let log = slog_scope::logger();
    let scaling_tasks = SCALING_TASKS.get().unwrap();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let server_backends = SERVER_BACKENDS.get().unwrap();

    // Stop the scaling task
    // Stop both the scaling task and image checker with write lock
    {
        let mut tasks = scaling_tasks.write().await;
        // Stop main scaling task
        if let Some(handle) = tasks.remove(service_name) {
            handle.abort();
            slog::debug!(log, "Scaling task aborted";
                "service" => service_name
            );
        }
        // Stop updater task if it exists
        let updater_key = format!("{}_updater", service_name);
        if let Some(handle) = tasks.remove(&updater_key) {
            handle.abort();
            slog::debug!(log, "Updater task aborted";
                "service" => service_name
            );
        }
    }

    // Stop the image check task with write lock
    {
        let image_check_tasks = IMAGE_CHECK_TASKS
            .get()
            .expect("Image check tasks not initialized");
        let mut tasks = image_check_tasks.write().await;
        if let Some(handle) = tasks.remove(service_name) {
            handle.abort();
            slog::debug!(log, "Image check task aborted";
                "service" => service_name
            );
        }
    }

    // Remove from load balancer
    {
        let mut backends_map = server_backends.write().await;
        backends_map.remove(service_name);
    }

    // Get instance data and remove from store with write lock
    let instances = {
        let mut store = instance_store.write().await;
        store.remove(service_name)
    };

    // Clean up instances if they exist
    if let Some(instances) = instances {
        for (uuid, metadata) in instances {
            let container_name = format!("{}__{}", service_name, uuid);
            let runtime = RUNTIME.get().unwrap().clone();

            // Remove container stats
            remove_container_stats(service_name, &container_name).await;

            // Handle health status
            if let Some(health_store) = CONTAINER_HEALTH.get() {
                let mut health_map = health_store.write().await;
                // Clone containers to avoid ownership issues
                let containers = metadata.containers.clone();
                for container in containers {
                    if let Some(status) = health_map.get_mut(&container.name) {
                        status.transition_to(
                            HealthState::Failed,
                            Some("Container being removed".to_string()),
                        );
                    }
                    health_map.remove(&container.name);
                    slog::debug!(log, "Removed health monitoring";
                        "service" => service_name,
                        "container" => &container.name
                    );
                }
            }

            // Stop each container in the metadata
            for container in &metadata.containers {
                if let Err(e) = runtime.stop_container(&container.name).await {
                    slog::error!(log, "Failed to stop container during service cleanup";
                        "service" => service_name,
                        "container" => &container.name,
                        "error" => e.to_string()
                    );
                } else {
                    slog::debug!(log, "Container stopped successfully";
                        "service" => service_name,
                        "container" => &container.name
                    );
                }
            }

            // Clean up network
            if let Err(e) = runtime
                .remove_pod_network(&metadata.network, service_name)
                .await
            {
                slog::error!(log, "Failed to remove network";
                    "service" => service_name,
                    "network" => &metadata.network,
                    "error" => e.to_string()
                );
            }
        }
    }

    slog::info!(log, "Service stopped and cleaned up"; "service" => service_name);
}

pub fn parse_memory_limit(memory_limit: &serde_json::Value) -> Result<u64> {
    match memory_limit {
        serde_json::Value::Number(num) => {
            let value = num
                .as_f64()
                .ok_or_else(|| anyhow!("Invalid memory number"))?;
            Ok((value * 1024.0 * 1024.0 * 1024.0) as u64) // Assume value is in GiB
        }
        serde_json::Value::String(s) => {
            let re = regex::Regex::new(r"^(\d+\.?\d*)([KMG]i?)$")?;
            if let Some(caps) = re.captures(s) {
                let value: f64 = caps[1].parse()?;
                let multiplier = match &caps[2] {
                    "Ki" | "K" => 1024.0,
                    "Mi" | "M" => 1024.0 * 1024.0,
                    "Gi" | "G" => 1024.0 * 1024.0 * 1024.0,
                    _ => return Err(anyhow!("Unsupported memory unit: {}", &caps[2])),
                };
                return Ok((value * multiplier) as u64);
            }
            Err(anyhow!("Invalid memory limit format: {}", s))
        }
        _ => Err(anyhow!("Unsupported memory limit type")),
    }
}

pub fn parse_cpu_limit(cpu_limit: &serde_json::Value) -> Result<u64> {
    match cpu_limit {
        serde_json::Value::Number(num) => {
            let value = num.as_f64().ok_or_else(|| anyhow!("Invalid CPU number"))?;
            if value <= 0.0 || value > ((u64::MAX as f64) / 1_000_000_000.0) {
                return Err(anyhow!("CPU limit out of valid range"));
            }
            Ok((value * 1_000_000_000.0) as u64)
        }
        serde_json::Value::String(s) => {
            let value: f64 = s.parse()?;
            Ok((value * 1_000_000_000.0) as u64)
        }
        _ => Err(anyhow!("Unsupported CPU limit type")),
    }
}

pub async fn handle_config_update(service_name: &str, config: ServiceConfig) -> Result<()> {
    let log = slog_scope::logger();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    // Validate service name format
    validate_service_name(&config.name)?;

    // Check for duplicate container names
    check_container_name_uniqueness(&config)?;

    // Validate ports within the service
    validate_service_ports(&config)?;

    // Check for conflicts with other services
    check_port_conflicts(&config, None).await?;

    // Only check for service name uniqueness if it's different from the current name
    if service_name != config.name {
        check_service_name_uniqueness(&config, Some(service_name)).await?;
    }

    slog::debug!(log, "Starting config update process";
        "service" => service_name,
        "thresholds" => format!("{:?}", config.resource_thresholds));

    // Check if this is a new service (no existing scaling task)
    let is_new_service = {
        let tasks = scaling_tasks.read().await;
        !tasks.contains_key(service_name)
    };

    if is_new_service {
        slog::info!(log, "Detected new service, initializing scaling task";
            "service" => service_name
        );

        // Initialize like a new service
        let service_name_clone = service_name.to_string();
        let handle = tokio::spawn(async move {
            auto_scale(service_name_clone).await;
        });

        // Store new task handle with write lock
        {
            let mut tasks = scaling_tasks.write().await;
            tasks.insert(service_name.to_string(), handle);
        }
    } else {
        // Existing service - send pause signal
        if let Some(sender) = CONFIG_UPDATES.get() {
            sender
                .send((service_name.to_string(), ScaleMessage::ConfigUpdate))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send config update: {}", e))?;
        }
    }

    // Update config in store
    if let Some(config_store) = CONFIG_STORE.get() {
        let mut store = config_store.write().await;
        // Iterate through FxHashMap and update the matching config
        for (_path, (_file_path, existing_config)) in store.iter_mut() {
            if existing_config.name == service_name {
                *existing_config = config.clone();
                break;
            }
        }
    }

    // Handle containers and proxy
    manage(service_name, config.clone()).await;
    proxy::run_proxy_for_service(service_name.to_string(), config.clone()).await;

    // If it's an existing service, send resume signal
    if !is_new_service {
        if let Some(sender) = CONFIG_UPDATES.get() {
            sender
                .send((service_name.to_string(), ScaleMessage::Resume))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send resume signal: {}", e))?;
        }
    }

    slog::debug!(log, "Completed config update process";
        "service" => service_name);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::scaling::manager::UnifiedScalingManager;
    use crate::container::scaling::manager::ScalingDecision;
    use std::collections::HashMap;
    use std::time::{Duration};
    use uuid::Uuid;
    use serde_json::Value;

    fn mock_service_config() -> ServiceConfig {
        ServiceConfig {
            name: "test_service".to_string(),
            network: Some("test_network".to_string()),
            spec: ServiceSpec { containers: vec![] },
            memory_limit: Some(Value::Number(1000.into())),
            pull_policy: None,
            cpu_limit: Some(Value::Number(2.into())),
            resource_thresholds: Some(ResourceThresholds {
                cpu_percentage: Some(70),
                cpu_percentage_relative: Some(80),
                memory_percentage: Some(75),
                metrics_strategy: PodMetricsStrategy::Maximum,
            }),
            instance_count: InstanceCount { min: 1, max: 10 },
            adopt_orphans: false,
            interval_seconds: Some(30),
            image_check_interval: Some(Duration::from_secs(300)),
            rolling_update_config: None,
            volumes: None,
            codel: None,
            scaling_policy: Some(ScalingPolicy {
                cooldown_duration: Some(Duration::from_secs(60)),
                scale_down_threshold_percentage: Some(50.0),
            }),
        }
    }

    #[test]
    fn test_scaling_policy_defaults() {
        let policy = ScalingPolicy::default();
        assert_eq!(policy.get_cooldown_duration(), Duration::from_secs(60));
        assert_eq!(policy.get_scale_down_threshold(), 50.0);
    }

    #[tokio::test]
    async fn test_unified_scaling_manager_no_change_on_cooldown() {
        let config = mock_service_config();
        let mut manager = UnifiedScalingManager::new(
            "test_service".to_string(),
            config,
            None,
            None,
        );

        let result = manager.evaluate(3, &HashMap::new()).await;
        assert!(matches!(result, ScalingDecision::NoChange));
    }

    #[tokio::test]
    async fn test_unified_scaling_manager_scale_up() {
        let config = mock_service_config();
        let mut manager = UnifiedScalingManager::new(
            "test_service".to_string(),
            config,
            None,
            None,
        );

        let mut pod_stats = HashMap::new();
        pod_stats.insert(Uuid::new_v4(), PodStats {
            cpu_percentage: 85.0,
            cpu_percentage_relative: 90.0,
            memory_usage: 900,
            memory_limit: 1000,
        });

        let result = manager.evaluate(3, &pod_stats).await;
        assert!(matches!(result, ScalingDecision::NoChange));
    }

    #[tokio::test]
    async fn test_unified_scaling_manager_scale_down() {
        let config = mock_service_config();
        let mut manager = UnifiedScalingManager::new(
            "test_service".to_string(),
            config,
            None,
            None,
        );

        let mut pod_stats = HashMap::new();
        pod_stats.insert(Uuid::new_v4(), PodStats {
            cpu_percentage: 10.0,
            cpu_percentage_relative: 15.0,
            memory_usage: 200,
            memory_limit: 1000,
        });

        let result = manager.evaluate(3, &pod_stats).await;
        assert!(matches!(result, ScalingDecision::NoChange));
    }

    #[test]
    fn test_service_config_instance_count() {
        let config = mock_service_config();
        assert_eq!(config.instance_count.min, 1);
        assert_eq!(config.instance_count.max, 10);
    }
}
