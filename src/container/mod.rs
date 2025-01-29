// src/container/mod.rs
pub mod rolling_update;
mod runtimes;
pub mod scale;
pub mod volumes;

pub use rolling_update::*;
pub use runtimes::*;
pub use scale::*;

use docker::DockerRuntime;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bollard::container::Stats;
use dashmap::DashMap;
use pingora_load_balancing::Backend;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::task::JoinHandle;
use uuid::Uuid;
use volumes::{detach_volume, VolumeData, VolumeMount};

use crate::api::status::update_instance_store_cache;
use crate::config::{
    get_config_by_service, parse_container_name, ResourceThresholds, ServiceConfig,
};
use crate::proxy::SERVER_BACKENDS;

const MAX_SERVICE_NAME_LENGTH: usize = 60; // Common k8s practice
const MAX_CONTAINER_NAME_LENGTH: usize = 60; // This gives us plenty of room

pub static IMAGE_CHECK_TASKS: OnceLock<DashMap<String, JoinHandle<()>>> = OnceLock::new();

// Update Container struct to include volume mounts
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Container {
    pub name: String,
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<ContainerPort>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_mounts: Option<Vec<VolumeMount>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_limit: Option<NetworkLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_thresholds: Option<ResourceThresholds>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NetworkLimit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingress_rate: Option<String>, // e.g. "10Mbps"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress_rate: Option<String>, // e.g. "5Mbps"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingress_burst: Option<String>, // e.g. "20Mb"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress_burst: Option<String>, // e.g. "10Mb"
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerPort {
    pub port: u16,
    pub target_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Protocol {
    TCP,
    UDP,
}

use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum ContainerError {
    #[error("Service name exceeds maximum length of {0} characters")]
    ServiceNameTooLong(usize),
    #[error("Container name exceeds maximum length of {0} characters")]
    ContainerNameTooLong(usize),
}

impl Container {
    pub fn generate_runtime_name(
        &self,
        service_name: &str,
        pod_number: u8,
        uuid: &str,
    ) -> Result<String, ContainerError> {
        if service_name.len() > MAX_SERVICE_NAME_LENGTH {
            return Err(ContainerError::ServiceNameTooLong(MAX_SERVICE_NAME_LENGTH).into());
        }
        if self.name.len() > MAX_CONTAINER_NAME_LENGTH {
            return Err(ContainerError::ContainerNameTooLong(MAX_CONTAINER_NAME_LENGTH).into());
        }

        // Format: service-name__pod-number__container-name__uuid
        Ok(format!(
            "{service_name}__{pod_number}__{}__{uuid}",
            self.name
        ))
    }
}

#[derive(Clone, Debug)]
pub struct ServiceStats {
    container_stats: DashMap<String, ContainerStats>,
}

impl ServiceStats {
    fn new() -> Self {
        Self {
            container_stats: DashMap::new(),
        }
    }

    pub fn update_stats(&self, container_name: &str, stats: ContainerStats) {
        self.container_stats
            .insert(container_name.to_string(), stats);
    }

    pub fn remove_container(&self, container_name: &str) {
        self.container_stats.remove(container_name);
    }

    pub fn get_container_stats(&self, container_name: &str) -> Option<ContainerStats> {
        self.container_stats.get(container_name).map(|s| s.clone())
    }
}

pub static SERVICE_STATS: OnceLock<DashMap<String, ServiceStats>> = OnceLock::new();

// Update the update_container_stats function to use service-level stats
pub async fn update_container_stats(
    service_name: &str,
    container_name: &str,
    stats: Stats,
    nano_cpus: Option<u64>,
) -> ContainerStats {
    let stats_store = CONTAINER_STATS.get().expect("Stats store not initialized");
    let service_stats = SERVICE_STATS.get().expect("Service stats not initialized");

    let now = SystemTime::now();
    let cpu_total = stats.cpu_stats.cpu_usage.total_usage;
    let system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
    let online_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;

    // Get previous stats with minimal lock time
    let previous_stats = stats_store.get(container_name).map(|entry| {
        (
            StatsEntry {
                timestamp: entry.timestamp,
                cpu_total_usage: entry.cpu_total_usage,
                system_cpu_usage: entry.system_cpu_usage,
            },
            // Get previous container stats for network rate calculation
            service_stats
                .get(service_name)
                .and_then(|s| s.get_container_stats(container_name)),
        )
    });

    let (cpu_percentage, cpu_percentage_relative) = calculate_cpu_percentages(
        previous_stats.as_ref().map(|(entry, _)| entry),
        cpu_total,
        system_cpu,
        online_cpus,
        nano_cpus,
    );

    // Update historical stats
    let stats_entry = StatsEntry {
        timestamp: now,
        cpu_total_usage: cpu_total,
        system_cpu_usage: system_cpu,
    };
    stats_store.insert(container_name.to_string(), stats_entry);

    let mut container_stats = ContainerStats {
        id: stats.id.clone(),
        cpu_percentage,
        cpu_percentage_relative,
        memory_usage: stats.memory_stats.usage.unwrap_or(0),
        memory_limit: stats.memory_stats.limit.unwrap_or(0),
        ip_address: String::from(""),
        port_mappings: HashMap::new(),
        network_rx_bytes: 0,
        network_tx_bytes: 0,
        network_rx_rate: 0.0,
        network_tx_rate: 0.0,
        timestamp: now,
    };

    // Update network stats using previous container stats if available
    container_stats.update_network_stats(
        &stats,
        previous_stats
            .as_ref()
            .map(|(_, prev)| prev.as_ref())
            .flatten(),
    );

    // Update service-level stats
    service_stats
        .entry(service_name.to_string())
        .or_insert_with(ServiceStats::new)
        .update_stats(container_name, container_stats.clone());

    container_stats
}

pub fn find_host_port(stats: &ContainerStats, container_port: u16) -> Option<u16> {
    stats.port_mappings.get(&container_port).copied()
}

// Update remove_container_stats to handle service-level cleanup
pub fn remove_container_stats(service_name: &str, container_name: &str) {
    if let Some(stats_store) = CONTAINER_STATS.get() {
        stats_store.remove(container_name);
    }

    if let Some(service_stats) = SERVICE_STATS.get() {
        if let Some(stats) = service_stats.get(service_name) {
            stats.remove_container(container_name);
        }
    }
}

// Initialize service stats in main.rs
pub fn initialize_stats() {
    SERVICE_STATS.get_or_init(DashMap::new);
    CONTAINER_STATS.get_or_init(DashMap::new);
}

pub static RUNTIME: OnceLock<Arc<dyn ContainerRuntime>> = OnceLock::new();

pub static INSTANCE_STORE: OnceLock<DashMap<String, HashMap<Uuid, InstanceMetadata>>> =
    OnceLock::new();

// Global registry for scaling tasks
pub static SCALING_TASKS: OnceLock<DashMap<String, JoinHandle<()>>> = OnceLock::new();

// Global stats history store
#[derive(Clone, Deserialize, Serialize)]
pub struct StatsEntry {
    pub timestamp: SystemTime,
    pub cpu_total_usage: u64,
    pub system_cpu_usage: u64,
}

// Global stats history store
pub static CONTAINER_STATS: OnceLock<DashMap<String, StatsEntry>> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerMetadata {
    pub name: String,
    pub network: String,
    pub ip_address: String,
    pub ports: Vec<ContainerPortMetadata>,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerPortMetadata {
    pub port: u16,                // Container's exposed port
    pub target_port: Option<u16>, // Optional target port
    pub node_port: Option<u16>,   // Optional external port
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstanceMetadata {
    pub uuid: Uuid,
    pub created_at: SystemTime,
    pub network: String,
    pub containers: Vec<ContainerMetadata>,
    pub image_hash: HashMap<String, String>, // container_name -> image_hash
}

// Container information struct
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerInfo {
    pub id: String,    // Container ID
    pub name: String,  // Container name
    pub state: String, // Container state (e.g., "running")
    pub port: u16,     // Exposed port, if available
}

// Struct to store container performance stats
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerStats {
    pub id: String,
    pub ip_address: String,
    pub cpu_percentage: f64,
    pub cpu_percentage_relative: f64,
    pub memory_usage: u64,
    pub memory_limit: u64,
    pub port_mappings: HashMap<u16, u16>,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub network_rx_rate: f64, // bytes per second
    pub network_tx_rate: f64, // bytes per second
    pub timestamp: SystemTime,
}

impl ContainerStats {
    pub fn update_network_stats(&mut self, stats: &Stats, previous: Option<&Self>) {
        if let Some(networks) = &stats.networks {
            let rx_bytes: u64 = networks.values().map(|net| net.rx_bytes).sum();
            let tx_bytes: u64 = networks.values().map(|net| net.tx_bytes).sum();

            // Calculate rates if we have previous stats
            if let Some(prev) = previous {
                let time_diff = self
                    .timestamp
                    .duration_since(prev.timestamp)
                    .unwrap_or_else(|_| Duration::from_secs(1))
                    .as_secs_f64();

                if time_diff > 0.0 {
                    self.network_rx_rate =
                        (rx_bytes as f64 - prev.network_rx_bytes as f64) / time_diff;
                    self.network_tx_rate =
                        (tx_bytes as f64 - prev.network_tx_bytes as f64) / time_diff;
                }
            }

            self.network_rx_bytes = rx_bytes;
            self.network_tx_bytes = tx_bytes;
        }
    }
}

// Define the container runtime trait
#[async_trait]
pub trait ContainerRuntime: Send + Sync + std::fmt::Debug {
    async fn check_image_updates(
        &self,
        service_name: &str,
        containers: &[Container],
        current_hashes: &HashMap<String, String>,
    ) -> Result<HashMap<String, bool>>;
    async fn get_image_digest(&self, image: &str) -> Result<String>;
    async fn remove_pod_network(&self, network_name: &str, service_name: &str) -> Result<()>;
    async fn create_pod_network(&self, service_name: &str, uuid: &str) -> Result<String>;
    async fn start_containers(
        &self,
        service_name: &str,
        pod_number: u8,
        containers: &Vec<Container>,
        service_config: &ServiceConfig,
    ) -> Result<Vec<(String, String, Vec<ContainerPortMetadata>)>>; // Returns vec of (container_name, ports)
    async fn stop_container(&self, name: &str) -> Result<()>;
    async fn inspect_container(&self, name: &str) -> Result<ContainerStats>;
    async fn list_containers(&self, service_name: Option<&str>) -> Result<Vec<ContainerInfo>>;
    async fn attempt_start_containers(
        &self,
        service_name: &str,
        pod_number: u8,
        containers: &Vec<Container>,
        service_config: &ServiceConfig,
    ) -> Result<Vec<(String, String, Vec<ContainerPortMetadata>)>>;
}

// Helper function to calculate CPU percentages
fn calculate_cpu_percentages(
    previous: Option<&StatsEntry>,
    cpu_total: u64,
    system_cpu: u64,
    online_cpus: f64,
    nano_cpus: Option<u64>,
) -> (f64, f64) {
    if let Some(previous) = previous {
        let cpu_delta = cpu_total as f64 - previous.cpu_total_usage as f64;
        let system_delta = system_cpu as f64 - previous.system_cpu_usage as f64;

        if system_delta > 0.0 && cpu_delta >= 0.0 {
            // Calculate absolute CPU percentage (across all cores)
            let absolute_cpu = ((cpu_delta / system_delta) * online_cpus * 100.0)
                .max(0.0)
                .min(100.0 * online_cpus);

            // Calculate relative CPU percentage
            let relative_cpu = if let Some(cpu_limit) = nano_cpus {
                // Convert nanocpus to CPU cores (1 CPU = 1_000_000_000 nanocpus)
                let allocated_cpu = cpu_limit as f64 / 1_000_000_000.0;
                if allocated_cpu > 0.0 {
                    // Calculate relative to allocated CPU
                    // Since absolute_cpu is across all cores, we need to compare with allocated_cpu * 100
                    let relative = (absolute_cpu / online_cpus) / allocated_cpu;
                    // Convert to percentage and clamp between 0-100
                    (relative * 100.0).max(0.0).min(100.0)
                } else {
                    0.0 // Avoid division by zero
                }
            } else {
                absolute_cpu / online_cpus // Normalize by number of CPUs if no limit
            };

            slog::trace!(slog_scope::logger(), "CPU calculation details";
                "cpu_delta" => cpu_delta,
                "system_delta" => system_delta,
                "absolute_cpu" => absolute_cpu,
                "relative_cpu" => relative_cpu,
                "online_cpus" => online_cpus,
                "allocated_cpu" => nano_cpus.map(|n| n as f64 / 1_000_000_000.0).unwrap_or(0.0)
            );

            (absolute_cpu / online_cpus, relative_cpu) // Normalize absolute CPU by cores
        } else {
            (0.0, 0.0)
        }
    } else {
        (0.0, 0.0) // First reading
    }
}

// Add helper function to parse network rates
pub fn parse_network_rate(rate: &str) -> Result<u64> {
    let re = regex::Regex::new(r"^(\d+(?:\.\d+)?)(Kbps|Mbps|Gbps)$")?;
    if let Some(caps) = re.captures(rate) {
        let value: f64 = caps[1].parse()?;
        let multiplier = match &caps[2] {
            "Kbps" => 1_000,
            "Mbps" => 1_000_000,
            "Gbps" => 1_000_000_000,
            _ => return Err(anyhow!("Unsupported network rate unit: {}", &caps[2])),
        };
        Ok((value * multiplier as f64) as u64)
    } else {
        Err(anyhow!("Invalid network rate format: {}", rate))
    }
}

pub fn create_runtime(runtime: &str) -> Result<Arc<dyn ContainerRuntime>> {
    match runtime {
        "docker" => Ok(Arc::new(DockerRuntime::new()?)),
        _ => Err(anyhow!("Unsupported runtime: {}", runtime)),
    }
}

pub async fn get_next_pod_number(service_name: &str) -> u8 {
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();

    match runtime.list_containers(Some(service_name)).await {
        Ok(containers) => containers
            .iter()
            .filter_map(|c| parse_container_name(&c.name).ok())
            .map(|parts| parts.pod_number)
            .max()
            .map_or(0, |max| max + 1),
        Err(_) => 0,
    }
}
pub async fn manage(service_name: &str, config: ServiceConfig) {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE.get().unwrap();
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();

    let current_instances = instance_store
        .get(service_name)
        .map(|entry| entry.value().len())
        .unwrap_or(0);

    let target_instances = config.instance_count.min as usize;
    let now = SystemTime::now();

    if current_instances < target_instances {
        slog::debug!(log, "Starting scale up";
            "service" => service_name,
            "current" => current_instances,
            "target" => target_instances
        );

        for _ in current_instances..target_instances {
            let pod_number = get_next_pod_number(service_name).await;
            let uuid = uuid::Uuid::new_v4();
            let network_name = format!("{}__{}", service_name, uuid);

            slog::debug!(log, "Starting new pod instance";
                "service" => service_name,
                "pod_number" => pod_number,
                "uuid" => uuid.to_string()
            );

            match runtime
                .start_containers(
                    service_name,
                    pod_number as u8,
                    &config.spec.containers,
                    &config,
                )
                .await
            {
                Ok(started_containers) => {
                    for (container_name, ip, ports) in &started_containers {
                        slog::debug!(log, "Container started successfully";
                            "service" => service_name,
                            "container" => container_name,
                            "ip" => ip,
                            "ports" => ?ports
                        );
                    }

                    if let Some(mut instances) = instance_store.get_mut(service_name) {
                        // Get image hashes for started containers
                        let mut image_hashes = HashMap::new();
                        for container in &config.spec.containers {
                            if let Ok(hash) = runtime.get_image_digest(&container.image).await {
                                image_hashes.insert(container.name.clone(), hash);
                            }
                        }

                        instances.insert(
                            uuid,
                            InstanceMetadata {
                                uuid,
                                created_at: now,
                                network: network_name.clone(),
                                image_hash: image_hashes.clone(),
                                containers: started_containers
                                    .into_iter()
                                    .map(|(name, ip, ports)| ContainerMetadata {
                                        name,
                                        network: network_name.clone(),
                                        ip_address: ip,
                                        ports,
                                        status: "running".to_string(),
                                    })
                                    .collect(),
                            },
                        );
                    } else {
                        let mut map = HashMap::new();
                        // Get image hashes for started containers
                        let mut image_hashes = HashMap::new();
                        for container in &config.spec.containers {
                            if let Ok(hash) = runtime.get_image_digest(&container.image).await {
                                image_hashes.insert(container.name.clone(), hash);
                            }
                        }

                        map.insert(
                            uuid,
                            InstanceMetadata {
                                uuid,
                                created_at: now,
                                network: network_name.clone(),
                                image_hash: image_hashes,
                                containers: started_containers
                                    .into_iter()
                                    .map(|(name, ip, ports)| ContainerMetadata {
                                        name,
                                        network: network_name.clone(),
                                        ip_address: ip,
                                        ports,
                                        status: "running".to_string(),
                                    })
                                    .collect(),
                            },
                        );
                        instance_store.insert(service_name.to_string(), map);
                    }

                    tokio::task::yield_now().await;
                }
                Err(e) => {
                    slog::error!(log, "Failed to start containers";
                        "service" => service_name,
                        "error" => e.to_string()
                    );
                }
            }
        }
    }
}

pub async fn clean_up(service_name: &str) {
    let log = slog_scope::logger();
    let instance_store = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised");
    let runtime = RUNTIME.get().expect("Runtime not initialised").clone();
    let scaling_tasks = SCALING_TASKS.get().unwrap();

    // Stop the auto-scaling task
    if let Some((_, handle)) = scaling_tasks.remove(service_name) {
        handle.abort();
        slog::trace!(log, "Scaling task aborted"; "service" => service_name);
    }

    if let Some((_, instances)) = instance_store.remove(service_name) {
        for (_uuid, metadata) in instances {
            // For each container in the pod
            for container in metadata.containers {
                // Detach volumes if any
                if let Some(config) = get_config_by_service(service_name) {
                    if let (Some(container_config), Some(volumes)) = (
                        config
                            .spec
                            .containers
                            .iter()
                            .find(|c| c.name == container.name),
                        &config.volumes,
                    ) {
                        if let Some(volume_mounts) = &container_config.volume_mounts {
                            for mount in volume_mounts.iter() {
                                if let Some(volume_data) = volumes.get(&mount.name) {
                                    if let Some(named_volume) = &volume_data.named_volume {
                                        if let Err(e) =
                                            detach_volume(&named_volume.name, &container.name).await
                                        {
                                            slog::error!(log, "Failed to detach volume";
                                                "service" => service_name,
                                                "container" => &container.name,
                                                "volume" => &named_volume.name,
                                                "error" => e.to_string()
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Remove from load balancer for each port
                for port_metadata in &container.ports {
                    if let Some(node_port) = port_metadata.node_port {
                        let proxy_key = format!("{}_{}", service_name, node_port);
                        if let Some(backends) = SERVER_BACKENDS.get().unwrap().get(&proxy_key) {
                            let addr = format!("{}:{}", container.ip_address, port_metadata.port);
                            if let Ok(backend) = Backend::new(&addr) {
                                backends.remove(&backend);
                                slog::debug!(log, "Removed backend from load balancer";
                                    "service" => service_name,
                                    "container" => &container.name,
                                    "port" => port_metadata.port,
                                    "node_port" => node_port
                                );
                            }
                        }
                    }
                }
                // Clean up stats for each container
                remove_container_stats(service_name, &container.name);

                // Stop each container
                let runtime = runtime.clone();
                if let Err(e) = runtime.stop_container(&container.name).await {
                    slog::error!(log, "Failed to stop container";
                        "service" => service_name,
                        "container" => &container.name,
                        "error" => e.to_string()
                    );
                }
            }
        }

        // Clean up entire service stats after all containers are stopped
        if let Some(service_stats) = SERVICE_STATS.get() {
            service_stats.remove(service_name);
        }
    }

    let _ = update_instance_store_cache();
}
