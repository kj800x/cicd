use crate::kubernetes::{DeployConfig, DeployConfigStatusBuilder};
use crate::prelude::*;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{DeleteParams, GroupVersionKind, PostParams, TypeMeta};
use kube::discovery::pinned_kind;
use kube::{
    api::{Api, DynamicObject, ListParams, Patch, PatchParams, ResourceExt},
    client::Client,
    core::discovery,
    Discovery,
};

pub async fn apply(client: &Client, ns: &str, obj: DynamicObject) -> AppResult<DynamicObject> {
    // require name + type info
    let name = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| AppError::Internal("metadata.name required".to_string()))?;
    let gvk = GroupVersionKind::try_from(
        obj.types
            .as_ref()
            .ok_or_else(|| AppError::Internal("missing types on DynamicObject".to_string()))?,
    )
    .map_err(|e| AppError::Internal(format!("failed parsing GVK: {}", e)))?;

    log::debug!("Applying {}/{}", ns, name);

    // resolve ApiResource and scope
    let (ar, caps) = pinned_kind(client, &gvk)
        .await
        .map_err(|e| AppError::Internal(format!("GVK {gvk:?} not found via discovery: {}", e)))?;

    let api: Api<DynamicObject> = match caps.scope {
        discovery::Scope::Namespaced => Api::namespaced_with(client.clone(), ns, &ar),
        discovery::Scope::Cluster => Api::all_with(client.clone(), &ar),
    };

    // SSA upsert
    let pp = PatchParams::apply("cicd-controller").force(); // drop .force() if you prefer conflicts to surface
    let obj = api
        .patch(&name, &pp, &Patch::Apply(obj))
        .await
        .map_err(AppError::Kubernetes)?;

    Ok(obj)
}

/// Delete a DynamicObject
pub async fn delete_dynamic_object(client: Client, obj: &DynamicObject) -> AppResult<()> {
    log::debug!(
        "Deleting {}/{}",
        obj.namespace().unwrap_or_else(|| "default".to_string()),
        obj.name_any()
    );

    let name = obj.name_any();
    let ns = obj.metadata.namespace.clone(); // may be None for cluster-scoped
    let gvk = GroupVersionKind::try_from(
        obj.types
            .as_ref()
            .ok_or_else(|| AppError::Internal("missing types".to_string()))?,
    )
    .map_err(|e| AppError::Internal(format!("failed parsing GVK: {}", e)))?;

    let (ar, caps) = pinned_kind(&client, &gvk).await?;

    let api: Api<DynamicObject> = match caps.scope {
        discovery::Scope::Namespaced => {
            let ns = ns.ok_or_else(|| {
                AppError::Internal("namespaced resource missing metadata.namespace".to_string())
            })?;
            Api::namespaced_with(client, &ns, &ar)
        }
        discovery::Scope::Cluster => Api::all_with(client, &ar),
    };

    let result = api.delete(&name, &DeleteParams::default()).await;
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(AppError::Internal(format!(
            "failed to delete object: {}",
            e
        ))),
    }
}

pub enum ListMode {
    All,
    Owned,
}

/// Return all DynamicObjects in `ns`
pub async fn list_namespace_objects(
    client: &Client,
    ns: &str,
    mode: ListMode,
) -> AppResult<Vec<DynamicObject>> {
    let disc = Discovery::new(client.clone()).run().await?;
    let mut out = Vec::new();

    for group in disc.groups() {
        for (ar, caps) in group.resources_by_stability() {
            // Only namespaced top-level resources (skip subresources like */status)
            if caps.scope != discovery::Scope::Namespaced || ar.plural.contains("/") {
                continue;
            }
            let types = TypeMeta {
                api_version: ar.api_version.clone(),
                kind: ar.kind.clone(),
            };

            let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);

            // Paginate to avoid truncation on large lists
            // only what *we* manage
            let mut lp = match mode {
                ListMode::All => ListParams::default().limit(500),
                ListMode::Owned => ListParams::default()
                    .labels("app.kubernetes.io/managed-by=cicd-controller")
                    .limit(500),
            };
            let mut continue_token: Option<String> = None;

            loop {
                if let Some(token) = continue_token.clone() {
                    lp = ListParams {
                        continue_token: Some(token),
                        ..lp.clone()
                    };
                }

                let res = api.list(&lp).await;
                let list = match res {
                    Ok(l) => l,
                    // 405 = method not allowed (common for subresources/misreported caps)
                    Err(kube::Error::Api(e)) if e.code == 405 => break,
                    // 403/404/etc.: skip this kind but keep going
                    Err(_) => break,
                };

                out.extend(list.items.into_iter().map(|mut o| {
                    o.types = o.types.or(Some(types.clone()));
                    o
                }));

                // FIXME: ChatGPT maybe suggests a "stall guard" (check to see if continue token is the same as the last one) to avoid k8s bugs.
                continue_token =
                    list.metadata
                        .continue_
                        .and_then(|x| if x.is_empty() { None } else { Some(x) });

                if continue_token.is_none() {
                    break;
                }
            }
        }
    }

    Ok(out)
}

pub async fn set_deploy_config_specs(
    client: &Client,
    namespace: &str,
    name: &str,
    specs: Vec<serde_json::Value>,
) -> AppResult<()> {
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);
    let patch = Patch::Merge(serde_json::json!({ "spec": { "specs": specs } }));
    let params = PatchParams::default();
    api.patch(name, &params, &patch)
        .await
        .map_err(AppError::Kubernetes)?;

    Ok(())
}

/// Update the DeployConfig status according to the given status builder
pub async fn update_deploy_config_status(
    client: &Client,
    namespace: &str,
    name: &str,
    update: DeployConfigStatusBuilder,
) -> AppResult<()> {
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);

    let status: serde_json::Value = update.into();
    let patch = Patch::Merge(&status);
    let params = PatchParams::default();
    api.patch_status(name, &params, &patch).await?;

    Ok(())
}

pub async fn delete_deploy_config(client: &Client, namespace: &str, name: &str) -> AppResult<()> {
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);
    api.delete(name, &DeleteParams::default()).await?;
    Ok(())
}

pub async fn get_all_deploy_configs(client: &Client) -> AppResult<Vec<DeployConfig>> {
    // Get all DeployConfigs across all namespaces
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            return Err(AppError::Kubernetes(e));
        }
    };

    Ok(deploy_configs)
}

pub async fn get_deploy_config(client: &Client, name: &str) -> AppResult<Option<DeployConfig>> {
    let deploy_configs = get_all_deploy_configs(client).await?;
    let deploy_config = deploy_configs
        .into_iter()
        .find(|config| config.name_any() == name);
    Ok(deploy_config)
}

pub async fn get_namespace_uid(client: &Client, namespace: &str) -> AppResult<String> {
    let namespaces_api: Api<Namespace> = Api::all(client.clone());
    let namespace = match namespaces_api.get(namespace).await {
        Ok(namespace) => namespace,
        Err(e) => {
            return Err(AppError::Kubernetes(e));
        }
    };

    #[allow(clippy::expect_used)]
    Ok(namespace
        .metadata
        .uid
        .expect("expect namespaces to all have a uid"))
}

/// Check if a namespace exists
pub async fn namespace_exists(client: &Client, namespace: &str) -> AppResult<bool> {
    let namespaces_api: Api<Namespace> = Api::all(client.clone());
    match namespaces_api.get(namespace).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
        Err(e) => Err(AppError::Kubernetes(e)),
    }
}

/// Create a namespace
pub async fn create_namespace(client: &Client, namespace: &str) -> AppResult<()> {
    let namespaces_api: Api<Namespace> = Api::all(client.clone());
    let ns = Namespace {
        metadata: kube::api::ObjectMeta {
            name: Some(namespace.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    namespaces_api
        .create(&PostParams::default(), &ns)
        .await
        .map_err(AppError::Kubernetes)?;
    log::info!("Created namespace: {}", namespace);
    Ok(())
}

/// Copy all resources from template namespace to target namespace
/// Skips resources that already exist in the target namespace
pub async fn copy_namespace_resources(
    client: &Client,
    template_ns: &str,
    target_ns: &str,
) -> AppResult<()> {
    log::info!(
        "Copying resources from template namespace {} to target namespace {}",
        template_ns,
        target_ns
    );

    // Get all resources from template namespace
    let template_resources = list_namespace_objects(client, template_ns, ListMode::All).await?;
    log::debug!(
        "Found {} resources in template namespace {}",
        template_resources.len(),
        template_ns
    );

    let mut copied_count = 0;
    let mut skipped_count = 0;

    for mut resource in template_resources {
        let resource_name = resource.name_any();
        let gvk = GroupVersionKind::try_from(
            resource
                .types
                .as_ref()
                .ok_or_else(|| AppError::Internal("missing types on DynamicObject".to_string()))?,
        )
        .map_err(|e| AppError::Internal(format!("failed parsing GVK: {}", e)))?;

        // Resolve ApiResource to check if resource exists in target namespace
        let (ar, caps) = pinned_kind(client, &gvk).await.map_err(|e| {
            AppError::Internal(format!("GVK {gvk:?} not found via discovery: {}", e))
        })?;

        let target_api: Api<DynamicObject> = match caps.scope {
            discovery::Scope::Namespaced => Api::namespaced_with(client.clone(), target_ns, &ar),
            discovery::Scope::Cluster => {
                // Skip cluster-scoped resources (they don't belong to a namespace)
                log::debug!(
                    "Skipping cluster-scoped resource {} (kind: {})",
                    resource_name,
                    gvk.kind
                );
                skipped_count += 1;
                continue;
            }
        };

        // Check if resource already exists in target namespace
        match target_api.get(&resource_name).await {
            Ok(_) => {
                log::debug!(
                    "Resource {}/{} already exists in target namespace, skipping",
                    target_ns,
                    resource_name
                );
                skipped_count += 1;
                continue;
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {
                // Resource doesn't exist, proceed with copying
            }
            Err(e) => {
                log::warn!(
                    "Error checking if resource {}/{} exists: {}",
                    target_ns,
                    resource_name,
                    e
                );
                continue;
            }
        }

        // Update resource metadata for target namespace
        resource.metadata.namespace = Some(target_ns.to_string());
        // Remove owner references and other namespace-specific metadata
        resource.metadata.owner_references = None;
        // Remove UID so Kubernetes can assign a new one
        resource.metadata.uid = None;
        // Remove resource version
        resource.metadata.resource_version = None;

        // Add labels to mark this resource as copied from template namespace
        let labels = resource
            .metadata
            .labels
            .get_or_insert_with(Default::default);
        labels.insert(
            "cicd.coolkev.com/copied-from-template".to_string(),
            "true".to_string(),
        );

        // Add annotations with template namespace and timestamp
        let annotations = resource
            .metadata
            .annotations
            .get_or_insert_with(Default::default);
        annotations.insert(
            "cicd.coolkev.com/copied-from-template-namespace".to_string(),
            template_ns.to_string(),
        );
        annotations.insert(
            "cicd.coolkev.com/copied-at".to_string(),
            chrono::Utc::now().to_rfc3339(),
        );

        // Copy the resource to target namespace
        match apply(client, target_ns, resource).await {
            Ok(_) => {
                log::debug!(
                    "Copied resource {}/{} from template namespace",
                    target_ns,
                    resource_name
                );
                copied_count += 1;
            }
            Err(e) => {
                log::warn!(
                    "Failed to copy resource {}/{}: {}",
                    target_ns,
                    resource_name,
                    e
                );
            }
        }
    }

    log::info!(
        "Finished copying resources: {} copied, {} skipped",
        copied_count,
        skipped_count
    );

    Ok(())
}

/// Ensure namespace exists, creating it if necessary and copying resources from template namespace
/// Returns true if namespace was newly created, false if it already existed
pub async fn ensure_namespace_exists(
    client: &Client,
    namespace: &str,
    template_namespace: Option<&str>,
) -> AppResult<bool> {
    // Check if namespace exists
    let exists = namespace_exists(client, namespace).await?;

    if exists {
        log::debug!("Namespace {} already exists", namespace);
        return Ok(false);
    }

    // Create namespace
    create_namespace(client, namespace).await?;

    // Copy resources from template namespace if provided
    if let Some(template_ns) = template_namespace {
        if template_ns == namespace {
            log::debug!(
                "Template namespace {} is the same as target namespace, skipping copy",
                template_ns
            );
        } else {
            match copy_namespace_resources(client, template_ns, namespace).await {
                Ok(_) => {
                    log::info!(
                        "Successfully copied resources from template namespace {} to {}",
                        template_ns,
                        namespace
                    );
                }
                Err(e) => {
                    // Log warning but don't fail - namespace was created successfully
                    log::warn!(
                        "Failed to copy resources from template namespace {} to {}: {}",
                        template_ns,
                        namespace,
                        e
                    );
                }
            }
        }
    }

    Ok(true)
}
