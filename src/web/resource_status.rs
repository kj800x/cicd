use std::{
    fmt::{self, Display},
    str::FromStr,
};

use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
use maud::{html, Markup};

use kube::{
    api::{DynamicObject, GroupVersionKind},
    Client, ResourceExt,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    error::{format_error_chain, AppError},
    kubernetes::{api::ListMode, list_namespace_objects, DeployConfig},
};

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

                deployment
                    .status
                    .as_ref()
                    .map(|status| {
                        html! {
                            span.m-left-2 {
                                "("
                                (status.ready_replicas.unwrap_or(0))
                                " / "
                                (deployment.spec.as_ref().map(|spec| spec.replicas.unwrap_or(0)).unwrap_or(0))
                                ")"
                            }
                        }
                    })
                    .unwrap_or_else(|| html! { "" })
            }
            HandledResourceKind::ReplicaSet => html! { "" },
            HandledResourceKind::Pod => {
                let pod = from_dynamic_object::<Pod>(obj).expect("Failed to deserialize Pod");

                pod.status
                    .as_ref()
                    .map(|status| {
                        html! {
                            span.m-left-2 {
                                "("
                                (status.phase.clone().unwrap_or("Unknown".to_string()))
                                ")"
                            }
                        }
                    })
                    .unwrap_or_else(|| html! { "" })
            }
            HandledResourceKind::Ingress => html! { "" },
            HandledResourceKind::Other(_) => html! { "" },
            HandledResourceKind::Service => html! { "" },
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
        let children = self.children(namespaced_objs);

        html! {
            ul {
                @for child in children {
                    (child.format(namespaced_objs))
                }
            }
        }
    }

    fn format(&self, namespaced_objs: &[DynamicObject]) -> Markup {
        html! {
            li {
                (self.format_self(namespaced_objs))
                (self.format_children(namespaced_objs))
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
    async fn format_resources(&self, client: &Client) -> Markup;
}

impl ResourceStatuses for DeployConfig {
    async fn format_resources(&self, client: &Client) -> Markup {
        let namespaced_objs = match list_namespace_objects(
            client.clone(),
            &self.namespace().unwrap_or_else(|| "default".to_string()),
            ListMode::All,
        )
        .await
        {
            Ok(objs) => objs,
            Err(e) => {
                return html! {
                    span { (format!("Kube spec parse error: {}", format_error_chain(&e))) }
                };
            }
        };

        html! {
            ul {
                @for resource in self.resource_specs() {
                    @match TryInto::<LiteResource>::try_into(resource) {
                        Ok(resource) => {
                            (resource.format(&namespaced_objs))
                        }
                        Err(e) => {
                            li {
                                (format!("Kube spec parse error: {}", format_error_chain(&e)))
                            }
                        }
                    }
                }
            }
        }
    }
}
