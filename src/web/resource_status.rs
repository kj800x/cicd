use std::{
    fmt::{self, Display},
    str::FromStr,
};

use futures_util::future::BoxFuture;
use maud::{html, Markup};

use kube::{
    api::{DynamicObject, GroupVersionKind},
    Client,
};
use serenity::FutureExt;

use crate::{
    error::{format_error_chain, AppError},
    kubernetes::{
        api::{get_dynamic_object, list_children},
        DeployConfig,
    },
};

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

#[derive(Clone)]
struct LiteResource {
    kind: HandledResourceKind,
    name: String,
    namespace: String,
}

impl LiteResource {
    async fn children(&self, client: &Client) -> Vec<LiteResource> {
        // Use the kube client to fetch the children (via owner_references) treating this as a DynamicObject.
        // Take the children and try them into LiteResource. (assume we implement TryFrom<DynamicObject> for LiteResource)
        let gvk: GroupVersionKind = match (&self.kind).try_into() {
            Ok(gvk) => gvk,
            Err(_) => return vec![],
        };

        // log::info!(
        //     "Getting children of {}: {}/{}",
        //     self.kind,
        //     self.namespace,
        //     self.name
        // );

        let obj = match get_dynamic_object(client, &self.namespace, &self.name, gvk).await {
            Ok(obj) => obj,
            Err(_) => return vec![],
        };

        // log::info!("Got object {:#?}", obj);

        let children = match list_children(client, &obj).await {
            Ok(children) => children,
            Err(_) => return vec![],
        };

        // log::info!("Got children of {:#?}", children);

        children
            .into_iter()
            .flat_map(|o| LiteResource::try_from(&o).ok())
            .collect()
    }

    fn format_self(&self) -> Markup {
        html! {
            span {
                b { (self.kind) }
                ": "
                (self.name)
            }
        }
    }

    fn format_children(&self, client: &Client) -> BoxFuture<'static, Markup> {
        let self_clone = self.clone();
        let client = client.clone();

        async move {
            let children = self_clone.children(&client).await;

            html! {
                ul {
                    @for child in children {
                        (child.format(&client).await)
                    }
                }
            }
        }
        .boxed()
    }

    async fn format(&self, client: &Client) -> Markup {
        html! {
            li {
                (self.format_self())
                (self.format_children(client).await)
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
    async fn format(&self, client: &Client) -> Markup;
}

impl ResourceStatuses for DeployConfig {
    async fn format(&self, client: &Client) -> Markup {
        html! {
            ul {
                @for resource in self.resource_specs() {
                    @match TryInto::<LiteResource>::try_into(resource) {
                        Ok(resource) => {
                            (resource.format(client).await)
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
