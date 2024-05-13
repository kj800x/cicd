// http://10.60.1.15:9000
// /api/endpoints
// /api/endpoints/2

use crate::{prelude::*, PortainerConfig};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct Container {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Names")]
    names: Vec<String>,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "ImageID")]
    image_id: String,
    #[serde(rename = "Command")]
    command: String,
    #[serde(rename = "Created")]
    created: u64,
    // #[serde(rename = "Ports")]
    // ports: Vec<Port>,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "Status")]
    status: String,
    // #[serde(rename = "HostConfig")]
    // host_config: HostConfig,
    // #[serde(rename = "NetworkSettings")]
    // network_settings: NetworkSettings,
    // #[serde(rename = "Mounts")]
    // mounts: Vec<Mount>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Image {
    #[serde(rename = "Containers")]
    containers: i64,
    #[serde(rename = "Created")]
    created: u64,
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
    #[serde(rename = "ParentId")]
    parent_id: String,
    #[serde(rename = "RepoDigests")]
    repo_digests: Vec<String>,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "SharedSize")]
    shared_size: i64,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "VirtualSize")]
    virtual_size: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DockerSnapshotRaw {
    #[serde(rename = "Containers")]
    containers: Vec<Container>,
    // #[serde(rename = "Volumes")]
    // volumes: Vec<Volume>,
    // #[serde(rename = "Networks")]
    // networks: Vec<Network>,
    #[serde(rename = "Images")]
    images: Vec<Image>,
    // #[serde(rename = "Info")]
    // info: Info,
    // #[serde(rename = "Version")]
    // version: Version,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DockerSnapshot {
    #[serde(rename = "Time")]
    time: u64,
    #[serde(rename = "DockerVersion")]
    docker_version: String,
    #[serde(rename = "Swarm")]
    swarm: bool,
    #[serde(rename = "TotalCPU")]
    total_cpu: u64,
    #[serde(rename = "TotalMemory")]
    total_memory: u64,
    #[serde(rename = "RunningContainerCount")]
    running_container_count: u64,
    #[serde(rename = "StoppedContainerCount")]
    stopped_container_count: u64,
    #[serde(rename = "HealthyContainerCount")]
    healthy_container_count: u64,
    #[serde(rename = "UnhealthyContainerCount")]
    unhealthy_container_count: u64,
    #[serde(rename = "VolumeCount")]
    volume_count: u64,
    #[serde(rename = "ImageCount")]
    image_count: u64,
    #[serde(rename = "ServiceCount")]
    service_count: u64,
    #[serde(rename = "StackCount")]
    stack_count: u64,
    #[serde(rename = "NodeCount")]
    node_count: u64,
    #[serde(rename = "GpuUseAll")]
    gpu_use_all: bool,
    #[serde(rename = "GpuUseList")]
    gpu_use_list: Vec<u64>,
    #[serde(rename = "DockerSnapshotRaw")]
    docker_snapshot_raw: DockerSnapshotRaw,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Endpoint {
    #[serde(rename = "Id")]
    id: u64,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Type")]
    r#type: u64,
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "GroupId")]
    group_id: u64,
    #[serde(rename = "PublicURL")]
    public_url: String,
    // #[serde(rename = "Gpus")]
    // gpus: Vec<u64>,
    // #[serde(rename = "TLSConfig")]
    // tls_config: TLSConfig,
    // #[serde(rename = "AzureCredentials")]
    // azure_credentials: AzureCredentials,
    // #[serde(rename = "TagIds")]
    // tag_ids: Vec<u64>,
    #[serde(rename = "Status")]
    status: u64,
    #[serde(rename = "Snapshots")]
    snapshots: Vec<DockerSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointBrief {
    #[serde(rename = "Id")]
    id: u64,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Type")]
    type_: u64,
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "GroupId")]
    group_id: u64,
    #[serde(rename = "PublicURL")]
    public_url: String,
    #[serde(rename = "Status")]
    status: u64,
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
