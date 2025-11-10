use std::{
    fmt::{self, Display},
    str::FromStr,
};

use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet as KReplicaSet},
    core::v1::{Pod, Service as KService},
    networking::v1::Ingress as KIngress,
};
use maud::{html, Markup};

use kube::{
    api::{DynamicObject, GroupVersionKind},
    ResourceExt,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    error::{format_error_chain, AppError},
    kubernetes::DeployConfig,
};

fn render_state_span(text: &str, level: &str) -> Markup {
    match level {
        "error" => html! {
            span.m-left-2.deployable-state.deployable-state--error { "(" (text) ")" }
        },
        "warn" => html! {
            span.m-left-2.deployable-state.deployable-state--warn { "(" (text) ")" }
        },
        "muted" => html! {
            span.m-left-2.deployable-state.deployable-state--muted { "(" (text) ")" }
        },
        _ => html! {
            span.m-left-2.deployable-state.deployable-state--neutral { "(" (text) ")" }
        },
    }
}

fn render_state_span_content(level: &str, inner: Markup) -> Markup {
    match level {
        "error" => html! {
            span.m-left-2.deployable-state.deployable-state--error { "(" (inner) ")" }
        },
        "warn" => html! {
            span.m-left-2.deployable-state.deployable-state--warn { "(" (inner) ")" }
        },
        "muted" => html! {
            span.m-left-2.deployable-state.deployable-state--muted { "(" (inner) ")" }
        },
        _ => html! {
            span.m-left-2.deployable-state.deployable-state--neutral { "(" (inner) ")" }
        },
    }
}

fn is_error_reason(reason: &str) -> bool {
    matches!(
        reason,
        "CrashLoopBackOff"
            | "ErrImagePull"
            | "ImagePullBackOff"
            | "CreateContainerConfigError"
            | "CreateContainerError"
            | "OOMKilled"
            | "RunContainerError"
            | "InvalidImageName"
            | "ContainerCannotRun"
            | "Evicted"
            | "NodeLost"
            | "Failed"
            | "ExitCode"
    )
}

fn is_warn_reason(reason: &str) -> bool {
    matches!(
        reason,
        "ContainerCreating"
            | "PodInitializing"
            | "Terminating"
            | "NotReady"
            | "Unschedulable"
            | "Pending"
            | "Reconciling"
            | "Updating"
            | "Waiting"
            | "Unknown"
    )
}

fn summarize_deployment_status_markup(deployment: &Deployment) -> Option<Markup> {
    let spec_replicas = deployment
        .spec
        .as_ref()
        .and_then(|s| s.replicas)
        .unwrap_or(0);
    let status = deployment.status.as_ref()?;

    let ready = status.ready_replicas.unwrap_or(0);
    let updated = status.updated_replicas.unwrap_or(0);
    let unavailable = status.unavailable_replicas.unwrap_or(0);
    let observed_generation = status.observed_generation.unwrap_or_default();
    let desired_generation = deployment.metadata.generation.unwrap_or_default();

    // Condition-based warnings take precedence
    if let Some(conditions) = &status.conditions {
        for c in conditions {
            // Progress deadline exceeded
            if c.type_ == "Progressing"
                && (c.reason.as_deref() == Some("ProgressDeadlineExceeded") || c.status == "False")
            {
                let reason = c
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Not progressing".to_string());
                return Some(render_state_span(
                    &format!("{} ({} ready / {})", reason, ready, spec_replicas),
                    "error",
                ));
            }
            // Replica failures
            if c.type_ == "ReplicaFailure" && c.status == "True" {
                let reason = c
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Replica failure".to_string());
                return Some(render_state_span(
                    &format!("{} ({} ready / {})", reason, ready, spec_replicas),
                    "error",
                ));
            }
        }
    }

    // Reconciling a new generation
    if observed_generation < desired_generation {
        return Some(render_state_span(
            &format!("Reconciling ({} ready / {})", ready, spec_replicas),
            "warn",
        ));
    }

    // Mid-rollout / updating
    if updated < spec_replicas {
        return Some(render_state_span(
            &format!("Updating {} / {}", updated, spec_replicas),
            "warn",
        ));
    }

    // Waiting for pods to become ready
    if ready < spec_replicas {
        return Some(render_state_span(
            &format!("Waiting {} / {}", ready, spec_replicas),
            "warn",
        ));
    }

    if unavailable > 0 {
        return Some(render_state_span(
            &format!("Unavailable {}", unavailable),
            "warn",
        ));
    }

    Some(render_state_span(
        &format!("Ready {} / {}", ready, spec_replicas),
        "neutral",
    ))
}

fn priority_for_reason(reason: &str) -> i32 {
    match reason {
        // Highest priority, actionable failures
        "CrashLoopBackOff" => 100,
        "ErrImagePull" => 95,
        "ImagePullBackOff" => 90,
        "CreateContainerConfigError" => 85,
        "CreateContainerError" => 80,
        "OOMKilled" => 75,
        "RunContainerError" => 70,
        "InvalidImageName" => 65,
        "ContainerCannotRun" => 60,
        // Scheduling / init / transitional states
        "Unschedulable" => 55,
        "ContainerCreating" => 50,
        "PodInitializing" => 45,
        "Terminating" => 40,
        "Evicted" => 35,
        "NodeLost" => 30,
        "NotReady" => 25,
        // Defaults
        _ => 10,
    }
}

fn summarize_pod_status_markup(pod: &Pod) -> Option<Markup> {
    // Pod-level terminal states first
    if pod.metadata.deletion_timestamp.is_some() {
        return Some(render_state_span("Terminating", "warn"));
    }

    let status = pod.status.as_ref()?;

    if let Some(r) = &status.reason {
        if r == "Evicted" {
            return Some(render_state_span("Evicted", "error"));
        }
    }

    // Unschedulable / scheduling issues
    if let Some(conditions) = &status.conditions {
        if let Some(cond) = conditions
            .iter()
            .find(|c| c.type_ == "PodScheduled" && c.status == "False")
        {
            let reason = cond
                .reason
                .clone()
                .unwrap_or_else(|| "Unschedulable".to_string());
            return Some(render_state_span(&reason, "warn"));
        }
    }

    // Gather container and initContainer signals
    #[derive(Clone)]
    struct Candidate {
        priority: i32,
        label: String,
        key: String,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    let mut consider_container = |init: bool, cs: &k8s_openapi::api::core::v1::ContainerStatus| {
        // Waiting reasons
        if let Some(state) = cs.state.as_ref() {
            if let Some(waiting) = state.waiting.as_ref() {
                let mut label = waiting
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Waiting".to_string());
                if init {
                    label = format!("Init {}", label);
                }
                let key = label.strip_prefix("Init ").unwrap_or(&label).to_string();
                candidates.push(Candidate {
                    priority: priority_for_reason(&label.replace("Init ", ""))
                        + if init { 1 } else { 0 },
                    label,
                    key,
                });
            } else if let Some(terminated) = state.terminated.as_ref() {
                let mut label = terminated
                    .reason
                    .clone()
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| {
                        if terminated.exit_code != 0 {
                            format!("ExitCode {}", terminated.exit_code)
                        } else {
                            "Terminated".to_string()
                        }
                    });
                if init {
                    label = format!("Init {}", label);
                }
                let mut key = label.strip_prefix("Init ").unwrap_or(&label).to_string();
                if key.starts_with("ExitCode") {
                    key = "ExitCode".to_string();
                }
                candidates.push(Candidate {
                    priority: priority_for_reason(&label.replace("Init ", ""))
                        + if init { 1 } else { 0 },
                    label,
                    key,
                });
            }
        }
        // OOMKilled in last_state
        if let Some(last) = cs.last_state.as_ref() {
            if let Some(terminated) = last.terminated.as_ref() {
                if let Some(r) = &terminated.reason {
                    if r == "OOMKilled" {
                        let mut label = "OOMKilled".to_string();
                        if init {
                            label = format!("Init {}", label);
                        }
                        let key = "OOMKilled".to_string();
                        candidates.push(Candidate {
                            priority: priority_for_reason("OOMKilled") + if init { 1 } else { 0 },
                            label,
                            key,
                        });
                    }
                }
            }
        }
        // Not ready running container
        if !cs.ready {
            let mut label = "NotReady".to_string();
            if init {
                label = format!("Init {}", label);
            }
            let key = "NotReady".to_string();
            candidates.push(Candidate {
                priority: priority_for_reason("NotReady") + if init { 1 } else { 0 },
                label,
                key,
            });
        }
    };

    if let Some(inits) = &status.init_container_statuses {
        for cs in inits {
            consider_container(true, cs);
        }
    }
    if let Some(containers) = &status.container_statuses {
        for cs in containers {
            consider_container(false, cs);
        }
    }

    // Select highest priority candidate if any
    if let Some(best) = candidates.into_iter().max_by_key(|c| c.priority) {
        let level = if is_error_reason(&best.key) {
            "error"
        } else if is_warn_reason(&best.key) {
            "warn"
        } else {
            "neutral"
        };
        return Some(render_state_span(&best.label, level));
    }

    // Fall back to Pod phase if present
    if let Some(phase) = &status.phase {
        let level = match phase.as_str() {
            "Running" | "Succeeded" => "neutral",
            "Failed" => "error",
            "Pending" | "Unknown" => "warn",
            _ => "warn",
        };
        return Some(render_state_span(phase, level));
    }

    Some(render_state_span("Unknown", "warn"))
}

fn summarize_replicaset_status_markup(rs: &KReplicaSet) -> Option<Markup> {
    let desired = rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let status = rs.status.as_ref();
    let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);
    // Grey out when scaled to zero
    if desired == 0 {
        return Some(render_state_span("0 / 0", "muted"));
    }
    if ready < desired {
        return Some(render_state_span(
            &format!("{} / {}", ready, desired),
            "warn",
        ));
    }
    Some(render_state_span(
        &format!("{} / {}", ready, desired),
        "neutral",
    ))
}

fn summarize_service_status_markup(svc: &KService) -> Option<Markup> {
    let spec = svc.spec.as_ref()?;
    let svc_type = spec
        .type_
        .clone()
        .unwrap_or_else(|| "ClusterIP".to_string());
    match svc_type.as_str() {
        "LoadBalancer" => {
            let ing = svc
                .status
                .as_ref()
                .and_then(|st| st.load_balancer.as_ref())
                .and_then(|lb| lb.ingress.as_ref());
            if let Some(entries) = ing {
                if entries.is_empty() {
                    Some(render_state_span("LB: pending", "warn"))
                } else {
                    let addrs: Vec<String> = entries
                        .iter()
                        .map(|e| {
                            e.hostname
                                .clone()
                                .or(e.ip.clone())
                                .unwrap_or_else(|| "-".to_string())
                        })
                        .collect();
                    Some(render_state_span(
                        &format!("LB: {}", addrs.join(", ")),
                        "neutral",
                    ))
                }
            } else {
                Some(render_state_span("LB: pending", "warn"))
            }
        }
        "NodePort" => {
            if let Some(ports) = &spec.ports {
                let nodes: Vec<String> = ports
                    .iter()
                    .filter_map(|p| p.node_port.map(|np| np.to_string()))
                    .collect();
                if nodes.is_empty() {
                    Some(render_state_span("NodePort", "warn"))
                } else {
                    Some(render_state_span(
                        &format!("NodePort: {}", nodes.join(",")),
                        "neutral",
                    ))
                }
            } else {
                Some(render_state_span("NodePort", "warn"))
            }
        }
        "ExternalName" => {
            let name = spec
                .external_name
                .clone()
                .unwrap_or_else(|| "-".to_string());
            Some(render_state_span(
                &format!("ExternalName: {}", name),
                "neutral",
            ))
        }
        // ClusterIP (default) / Headless
        _ => {
            let cip = spec.cluster_ip.clone();
            if let Some(ci) = cip {
                if ci == "None" {
                    Some(render_state_span("Headless", "neutral"))
                } else {
                    Some(render_state_span(&format!("ClusterIP: {}", ci), "neutral"))
                }
            } else {
                Some(render_state_span("ClusterIP: -", "warn"))
            }
        }
    }
}

fn summarize_ingress_status_markup(ing: &KIngress) -> Option<Markup> {
    fn extract_external_dns_hostname(ing: &KIngress) -> Option<String> {
        let anns = ing.metadata.annotations.as_ref()?;
        let value = anns.get("external-dns.alpha.kubernetes.io/hostname")?;
        let first = value.split(',').next().map(|s| s.trim().to_string())?;
        if first.is_empty() {
            None
        } else {
            Some(first)
        }
    }

    let ext_host = extract_external_dns_hostname(ing);
    let lb_ing = ing
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref());

    if let Some(entries) = lb_ing {
        if entries.is_empty() {
            if let Some(host) = ext_host {
                let link = html! {
                    "LB: pending · "
                    a href=(format!("https://{}", host)) target="_blank" rel="noopener noreferrer" { (host) }
                };
                return Some(render_state_span_content("warn", link));
            } else {
                return Some(render_state_span("LB: pending", "warn"));
            }
        }
        let addrs: Vec<String> = entries
            .iter()
            .map(|e| {
                e.hostname
                    .clone()
                    .or(e.ip.clone())
                    .unwrap_or_else(|| "-".to_string())
            })
            .collect();
        if let Some(host) = ext_host {
            let content = html! {
                (format!("LB: {}", addrs.join(", "))) " · "
                a href=(format!("https://{}", host)) target="_blank" rel="noopener noreferrer" { (host) }
            };
            return Some(render_state_span_content("neutral", content));
        } else {
            return Some(render_state_span(
                &format!("LB: {}", addrs.join(", ")),
                "neutral",
            ));
        }
    }

    // If there are rules defined but no LB yet, warn (common during provisioning)
    let rules_len = ing
        .spec
        .as_ref()
        .and_then(|s| s.rules.as_ref())
        .map(|r| r.len())
        .unwrap_or(0);
    if rules_len > 0 {
        if let Some(host) = ext_host {
            let link = html! {
                "LB: pending · "
                a href=(format!("https://{}", host)) target="_blank" rel="noopener noreferrer" { (host) }
            };
            return Some(render_state_span_content("warn", link));
        } else {
            return Some(render_state_span("LB: pending", "warn"));
        }
    }

    // Otherwise, neutral minimal info
    if let Some(host) = ext_host {
        let link = html! {
            "Ingress · "
            a href=(format!("https://{}", host)) target="_blank" rel="noopener noreferrer" { (host) }
        };
        Some(render_state_span_content("neutral", link))
    } else {
        Some(render_state_span("Ingress", "neutral"))
    }
}
fn from_dynamic_object<T: DeserializeOwned>(obj: &DynamicObject) -> Result<T, AppError> {
    let value: Value = serde_json::to_value(obj)
        .map_err(|e| AppError::Internal(format!("Failed to serialize DynamicObject: {}", e)))?;
    serde_json::from_value(value)
        .map_err(|e| AppError::Internal(format!("Failed to deserialize DynamicObject: {}", e)))
}

fn list_children(obj: &DynamicObject, namespaced_objs: &[DynamicObject]) -> Vec<DynamicObject> {
    let Some(obj_uid) = &obj.metadata.uid else {
        return vec![];
    };

    namespaced_objs
        .iter()
        .filter(|o| {
            o.metadata
                .owner_references
                .as_ref()
                .unwrap_or(&Vec::new())
                .iter()
                .any(|or| or.uid == *obj_uid)
        })
        .cloned()
        .collect()
}

#[derive(Clone)]
enum HandledResourceKind {
    Deployment,
    ReplicaSet,
    Pod,
    Service,
    Ingress,
    Other(String),
}

impl FromStr for HandledResourceKind {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Deployment" => Ok(HandledResourceKind::Deployment),
            "ReplicaSet" => Ok(HandledResourceKind::ReplicaSet),
            "Pod" => Ok(HandledResourceKind::Pod),
            "Service" => Ok(HandledResourceKind::Service),
            "Ingress" => Ok(HandledResourceKind::Ingress),
            s => Ok(HandledResourceKind::Other(s.to_string())),
        }
    }
}

impl Display for HandledResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandledResourceKind::Deployment => write!(f, "Deployment"),
            HandledResourceKind::ReplicaSet => write!(f, "ReplicaSet"),
            HandledResourceKind::Pod => write!(f, "Pod"),
            HandledResourceKind::Service => write!(f, "Service"),
            HandledResourceKind::Ingress => write!(f, "Ingress"),
            HandledResourceKind::Other(s) => write!(f, "{}", s),
        }
    }
}

impl TryInto<GroupVersionKind> for &HandledResourceKind {
    type Error = AppError;

    fn try_into(self) -> Result<GroupVersionKind, Self::Error> {
        match self {
            HandledResourceKind::Deployment => {
                Ok(GroupVersionKind::gvk("apps", "v1", "Deployment"))
            }
            HandledResourceKind::ReplicaSet => {
                Ok(GroupVersionKind::gvk("apps", "v1", "ReplicaSet"))
            }
            HandledResourceKind::Pod => Ok(GroupVersionKind::gvk("", "v1", "Pod")),
            HandledResourceKind::Service => Ok(GroupVersionKind::gvk("", "v1", "Service")),
            HandledResourceKind::Ingress => {
                Ok(GroupVersionKind::gvk("networking.k8s.io", "v1", "Ingress"))
            }
            HandledResourceKind::Other(s) => {
                Err(AppError::Internal(format!("Unknown resource kind: {}", s)))
            }
        }
    }
}

impl HandledResourceKind {
    #[allow(clippy::expect_used)]
    pub fn format_status(&self, obj: &DynamicObject) -> Markup {
        match self {
            HandledResourceKind::Deployment => {
                let deployment = from_dynamic_object::<Deployment>(obj)
                    .expect("Failed to deserialize Deployment");
                let markup = summarize_deployment_status_markup(&deployment);
                match markup {
                    Some(m) => m,
                    None => html! { "" },
                }
            }
            HandledResourceKind::ReplicaSet => {
                let rs = from_dynamic_object::<KReplicaSet>(obj)
                    .expect("Failed to deserialize ReplicaSet");
                summarize_replicaset_status_markup(&rs).unwrap_or_else(|| html! { "" })
            }
            HandledResourceKind::Pod => {
                let pod = from_dynamic_object::<Pod>(obj).expect("Failed to deserialize Pod");

                let markup = summarize_pod_status_markup(&pod);
                match markup {
                    Some(m) => m,
                    None => html! { "" },
                }
            }
            HandledResourceKind::Ingress => {
                let ing =
                    from_dynamic_object::<KIngress>(obj).expect("Failed to deserialize Ingress");
                summarize_ingress_status_markup(&ing).unwrap_or_else(|| html! { "" })
            }
            HandledResourceKind::Other(_) => html! { "" },
            HandledResourceKind::Service => {
                let svc =
                    from_dynamic_object::<KService>(obj).expect("Failed to deserialize Service");
                summarize_service_status_markup(&svc).unwrap_or_else(|| html! { "" })
            }
        }
    }
}

#[derive(Clone)]
struct LiteResource {
    kind: HandledResourceKind,
    name: String,
    namespace: String,
}

impl LiteResource {
    fn is_scaled_to_zero_replicaset(&self, namespaced_objs: &[DynamicObject]) -> bool {
        if !matches!(self.kind, HandledResourceKind::ReplicaSet) {
            return false;
        }
        // Find this object in the prefetched list
        let obj = namespaced_objs.iter().find(|o| {
            o.name_any() == self.name
                && o.namespace().as_deref() == Some(&self.namespace)
                && o.types.as_ref().map(|t| t.kind.as_str()) == Some(&self.kind.to_string())
        });
        let Some(obj) = obj else {
            return false;
        };
        if let Ok(rs) = from_dynamic_object::<KReplicaSet>(obj) {
            let desired = rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
            return desired == 0;
        }
        false
    }
    fn children(&self, namespaced_objs: &[DynamicObject]) -> Vec<LiteResource> {
        // Find this object in the prefetched list
        let obj = namespaced_objs.iter().find(|o| {
            o.name_any() == self.name
                && o.namespace().as_deref() == Some(&self.namespace)
                && o.types.as_ref().map(|t| t.kind.as_str()) == Some(&self.kind.to_string())
        });

        let Some(obj) = obj else {
            return vec![];
        };

        // Get children from prefetched list
        let children = list_children(obj, namespaced_objs);

        children
            .iter()
            .flat_map(|o| LiteResource::try_from(o).ok())
            .collect()
    }

    fn format_self_status(&self, namespaced_objs: &[DynamicObject]) -> Markup {
        // Find this object in the prefetched list
        let obj = namespaced_objs.iter().find(|o| {
            o.name_any() == self.name
                && o.namespace().as_deref() == Some(&self.namespace)
                && o.types.as_ref().map(|t| t.kind.as_str()) == Some(&self.kind.to_string())
        });

        let Some(obj) = obj else {
            return html! {};
        };

        self.kind.format_status(obj)
    }

    fn format_self(&self, namespaced_objs: &[DynamicObject]) -> Markup {
        let status = self.format_self_status(namespaced_objs);

        html! {
            span {
                b { (self.kind) }
                ": "
                (self.name)
                (status)
            }
        }
    }

    fn format_children(&self, namespaced_objs: &[DynamicObject]) -> Markup {
        let mut children = self.children(namespaced_objs);
        // Stable sort so ReplicaSets scaled to zero appear last
        children.sort_by_key(|c| {
            if c.is_scaled_to_zero_replicaset(namespaced_objs) {
                1u8
            } else {
                0u8
            }
        });

        html! {
            ul.deployable-item__child-list {
                @for child in children {
                    (child.format(namespaced_objs))
                }
            }
        }
    }

    fn format(&self, namespaced_objs: &[DynamicObject]) -> Markup {
        let muted = self.is_scaled_to_zero_replicaset(namespaced_objs);
        html! {
            @if muted {
                li.deployables-tree__item.deployables-tree__item--muted {
                    (self.format_self(namespaced_objs))
                    (self.format_children(namespaced_objs))
                }
            } @else {
                li.deployables-tree__item {
                    (self.format_self(namespaced_objs))
                    (self.format_children(namespaced_objs))
                }
            }
        }
    }
}

impl TryFrom<&serde_json::Value> for LiteResource {
    type Error = AppError;

    fn try_from(value: &serde_json::Value) -> Result<Self, Self::Error> {
        let kind = value
            .get("kind")
            .and_then(|k| k.as_str().map(HandledResourceKind::from_str))
            .ok_or(AppError::Internal("kind is required".to_string()))??;

        let name = value
            .get("metadata")
            .and_then(|m| {
                m.get("name")
                    .and_then(|n| n.as_str().map(|n| n.to_string()))
            })
            .ok_or(AppError::Internal("name is required".to_string()))?;

        let namespace = value
            .get("metadata")
            .and_then(|m| {
                m.get("namespace")
                    .and_then(|n| n.as_str().map(|n| n.to_string()))
            })
            .ok_or(AppError::Internal("namespace is required".to_string()))?;

        Ok(Self {
            kind,
            name,
            namespace,
        })
    }
}

impl TryFrom<&DynamicObject> for LiteResource {
    type Error = AppError;

    // FIXME: Can we just do this directly instead of going via serde?
    fn try_from(value: &DynamicObject) -> Result<Self, Self::Error> {
        let value = serde_json::to_value(value)
            .map_err(|e| AppError::Internal(format!("Failed to serialize DynamicObject: {}", e)))?;
        LiteResource::try_from(&value)
    }
}

pub trait ResourceStatuses {
    async fn format_resources(&self, namespaced_objs: &[DynamicObject]) -> Markup;
}

impl ResourceStatuses for DeployConfig {
    async fn format_resources(&self, namespaced_objs: &[DynamicObject]) -> Markup {
        html! {
            ul.deployable-item__child-list {
                @for resource in self.resource_specs() {
                    @match TryInto::<LiteResource>::try_into(resource) {
                        Ok(resource) => {
                            (resource.format(&namespaced_objs))
                        }
                        Err(e) => {
                            li.deployables-tree__item {
                                (format!("Kube spec parse error: {}", format_error_chain(&e)))
                            }
                        }
                    }
                }
            }
        }
    }
}
