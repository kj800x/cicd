use crate::kubernetes::{DeployConfig, DeployConfigStatusBuilder};
use crate::prelude::*;
use kube::api::{DeleteParams, GroupVersionKind, TypeMeta};
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

/// Return all DynamicObjects in `ns`
pub async fn list_namespace_objects(client: Client, ns: &str) -> AppResult<Vec<DynamicObject>> {
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
            let mut lp = ListParams::default()
                .labels("app.kubernetes.io/managed-by=cicd-controller")
                .limit(500);
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
