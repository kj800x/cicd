// http://10.60.1.15:9000
// /api/endpoints
// /api/endpoints/2

use crate::{prelude::*, PortainerConfig};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct Container {
    Id: String,
    Names: Vec<String>,
    Image: String,
    ImageID: String,
    Command: String,
    Created: u64,
    // Ports: Vec<Port>,
    Labels: Option<HashMap<String, String>>,
    State: String,
    Status: String,
    // HostConfig: HostConfig,
    // NetworkSettings: NetworkSettings,
    // Mounts: Vec<Mount>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Image {
    Containers: i64,
    Created: u64,
    Id: String,
    Labels: Option<HashMap<String, String>>,
    ParentId: String,
    RepoDigests: Vec<String>,
    RepoTags: Option<Vec<String>>,
    SharedSize: i64,
    Size: i64,
    VirtualSize: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DockerSnapshotRaw {
    Containers: Vec<Container>,
    // Volumes: Vec<Volume>,
    // Networks: Vec<Network>,
    Images: Vec<Image>,
    // Info: Info,
    // Version: Version,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DockerSnapshot {
    Time: u64,
    DockerVersion: String,
    Swarm: bool,
    TotalCPU: u64,
    TotalMemory: u64,
    RunningContainerCount: u64,
    StoppedContainerCount: u64,
    HealthyContainerCount: u64,
    UnhealthyContainerCount: u64,
    VolumeCount: u64,
    ImageCount: u64,
    ServiceCount: u64,
    StackCount: u64,
    NodeCount: u64,
    GpuUseAll: bool,
    GpuUseList: Vec<u64>,
    DockerSnapshotRaw: DockerSnapshotRaw,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Endpoint {
    Id: u64,
    Name: String,
    Type: u64,
    URL: String,
    GroupId: u64,
    PublicURL: String,
    // Gpus: Vec<u64>,
    // TLSConfig: TLSConfig,
    // AzureCredentials: AzureCredentials,
    // TagIds: Vec<u64>,
    Status: u64,
    Snapshots: Vec<DockerSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointBrief {
    Id: u64,
    Name: String,
    Type: u64,
    URL: String,
    GroupId: u64,
    PublicURL: String,
    Status: u64,
}

pub async fn get_endpoint(
    id: u64,
    portainer_config: PortainerConfig,
) -> Result<Endpoint, reqwest::Error> {
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}api/endpoints/{}", portainer_config.base, id))
        .header("X-API-Key", portainer_config.api_key)
        .send()
        .await?;
    let endpoint: Endpoint = res.json().await?;
    Ok(endpoint)
}

pub async fn get_endpoints(
    portainer_config: PortainerConfig,
) -> Result<Vec<EndpointBrief>, reqwest::Error> {
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}api/endpoints", portainer_config.base))
        .header("X-API-Key", portainer_config.api_key)
        .send()
        .await?;
    let endpoints: Vec<EndpointBrief> = res.json().await?;
    Ok(endpoints)
}
