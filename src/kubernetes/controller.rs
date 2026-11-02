use super::DeployConfig;
use crate::error::format_error_chain;
use crate::kubernetes::api::list_namespace_objects_live;
use crate::kubernetes::api::ListMode;
use crate::kubernetes::cr_writers::set_reconcile_status;
use crate::kubernetes::deploy_config::{ReconcileError, ReconcileStatus};
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::{apply, delete_dynamic_object, ensure_namespace_exists};
use crate::prelude::*;
use futures_util::StreamExt;
use kube::{
    api::{Api, DynamicObject, ResourceExt},
    client::Client,
    runtime::{controller::Action, watcher, Controller},
};
use std::{sync::Arc, time::Duration};

/// How often an unchanged config is reconciled again to catch drift.
const RESYNC: Duration = Duration::from_secs(300);

/// Context for the controller
#[derive(Clone)]
pub struct ControllerContext {
    /// Kubernetes client
    client: Client,
}

/// The reconciliation function for DeployConfig resources
async fn reconcile(dc: Arc<DeployConfig>, ctx: Arc<ControllerContext>) -> AppResult<Action> {
    let client = &ctx.client;
    let ns = dc.namespace().unwrap_or_else(|| "default".to_string());
    let name = dc.name_any();

    log::debug!("Reconciling DeployConfig {}/{}", ns, name);

    // Ensure namespace exists (safety check - DeployConfig should already exist in target namespace)
    let template_namespace = std::env::var("TEMPLATE_NAMESPACE").ok();
    if let Err(e) = ensure_namespace_exists(client, &ns, template_namespace.as_deref()).await {
        log::warn!(
            "Failed to ensure namespace {} exists during reconciliation: {}",
            ns,
            e
        );
        // Continue anyway - namespace might already exist
    }

    // A deploy writes the specs, then the status. Rendering in between would
    // apply the new manifests with the old values (and pruning would act
    // on them); the status write reconciles again through the watch.
    if dc.deploy_in_flight() {
        log::info!(
            "DeployConfig {}/{}: deploy in progress (specs are ahead of the status); not rendering yet",
            ns,
            name
        );
        return Ok(Action::requeue(Duration::from_secs(5)));
    }

    if dc.deployment_state() == DeploymentState::Undeployed && !dc.resource_specs().is_empty() {
        log::debug!(
            "DeployConfig {}/{} is undeployed; not applying its {} manifest templates",
            ns,
            name,
            dc.resource_specs().len()
        );
    }
    // Every declared parameter the templates use must have a deployed
    // value; otherwise nothing is applied or pruned, and the error is
    // recorded on the status.
    let children = match dc.desired_children() {
        Ok(children) => children,
        Err(e) => {
            let failure = Failure {
                message: format!("the config does not render: {e}"),
                resources: vec![],
                permanent: true,
            };
            record(client, &dc, None, Some(&failure)).await;
            return Err(e);
        }
    };
    let render_hash = DeployConfig::render_hash(&children);

    // Apply every resource, even after one fails: one bad manifest must not
    // hold back the rest of the config.
    let mut failures: Vec<(String, AppError)> = Vec::new();
    for obj in children {
        let label = resource_label(&obj);
        if let Err(e) = apply(client, &ns, obj).await {
            log::error!(
                "DeployConfig {}/{}: failed to apply {}: {}",
                ns,
                name,
                label,
                format_error_chain(&e)
            );
            failures.push((label, e));
        }
    }
    let failure = Failure::from_applies(&failures);
    record(client, &dc, Some(render_hash), failure.as_ref()).await;
    if let Some(failure) = failure {
        // A resource that failed to apply still carries the previous
        // deploy's annotations, so the prune below would see it as stale
        // and delete it. Nothing is pruned until every resource applies.
        log::warn!(
            "DeployConfig {}/{}: {} of its resources failed to apply; skipping prune",
            ns,
            name,
            failure.resources.len()
        );
        // A rejected manifest will not apply until a new deploy changes it,
        // and that change reconciles at once through the watch.
        let retry = if failure.permanent {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(5)
        };
        return Ok(Action::requeue(retry));
    }

    // Prune stale resources
    log::debug!("Pruning stale resources...");
    // Live, never from the cache: this list decides what gets deleted.
    let objects = list_namespace_objects_live(client, &ns, ListMode::Owned).await?;
    log::debug!("Got objects in namespace {}/{}", ns, name);
    log::trace!("Objects: {objects:#?}");
    let stale_objects: Vec<DynamicObject> = objects
        .into_iter()
        .filter(|o| dc.owns(o))
        .filter(|o| !dc.child_is_up_to_date(o))
        .collect();
    log::debug!("Stale objects: {stale_objects:#?}");
    let prune_disabled = prune_disabled();
    for object in stale_objects {
        if prune_disabled {
            log::warn!(
                "Prune disabled: would delete stale resource {}/{} ({}) owned by DeployConfig {}",
                ns,
                object.name_any(),
                object
                    .types
                    .as_ref()
                    .map(|t| t.kind.as_str())
                    .unwrap_or("?"),
                name
            );
            continue;
        }
        log::debug!("Deleting stale resource {}/{}", ns, object.name_any());
        delete_dynamic_object(client.clone(), &object).await?;
    }
    log::debug!("Pruning stale resources complete");

    // Requeue reconciliation
    // Changes to the DeployConfig reconcile immediately through the watch;
    // this requeue only catches drift in the children. Five seconds across
    // sixty configs, each listing every kind in its namespace, was most of
    // the API server's load.
    Ok(Action::requeue(RESYNC))
}

/// Why a reconcile could not apply a config, before it is recorded.
struct Failure {
    message: String,
    resources: Vec<String>,
    permanent: bool,
}

impl Failure {
    fn from_applies(failures: &[(String, AppError)]) -> Option<Failure> {
        if failures.is_empty() {
            return None;
        }
        Some(Failure {
            message: failures
                .iter()
                .map(|(label, e)| format!("{label}: {}", apply_error_message(e)))
                .collect::<Vec<_>>()
                .join("\n"),
            resources: failures.iter().map(|(label, _)| label.clone()).collect(),
            permanent: failures.iter().any(|(_, e)| is_permanent(e)),
        })
    }
}

fn resource_label(obj: &DynamicObject) -> String {
    format!(
        "{}/{}",
        obj.types.as_ref().map(|t| t.kind.as_str()).unwrap_or("?"),
        obj.name_any()
    )
}

/// The API server's own message for a rejected request, without the
/// wrapper text around it; anything else as displayed.
fn apply_error_message(e: &AppError) -> String {
    match e {
        AppError::Kubernetes(kube::Error::Api(response)) => {
            format!("{} ({})", response.message, response.code)
        }
        other => other.to_string(),
    }
}

/// A 4xx other than a conflict (409) or throttling (429): the API server
/// refused the object itself, so the same render will keep failing.
fn is_permanent(e: &AppError) -> bool {
    match e {
        AppError::Kubernetes(kube::Error::Api(response)) => {
            (400..500).contains(&response.code) && response.code != 409 && response.code != 429
        }
        _ => false,
    }
}

/// The status to record for this reconcile. A failure that continues an
/// earlier one keeps its `since`.
fn reconcile_status(
    previous: Option<&ReconcileStatus>,
    render_hash: Option<String>,
    failure: Option<&Failure>,
    now: &str,
) -> ReconcileStatus {
    let error = failure.map(|f| ReconcileError {
        message: f.message.clone(),
        since: previous
            .and_then(|p| p.error.as_ref())
            .map(|e| e.since.clone())
            .unwrap_or_else(|| now.to_string()),
        resources: f.resources.clone(),
        permanent: f.permanent,
    });
    ReconcileStatus { render_hash, error }
}

/// Write the reconcile outcome to the DeployConfig's status when it
/// differs from what is there. Unchanged outcomes are not written: every
/// write is a watch event and would reconcile again.
async fn record(
    client: &Client,
    dc: &DeployConfig,
    render_hash: Option<String>,
    failure: Option<&Failure>,
) {
    let previous = dc.reconcile_status();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let next = reconcile_status(previous, render_hash, failure, &now);
    if previous == Some(&next) {
        return;
    }
    let ns = dc.namespace().unwrap_or_else(|| "default".to_string());
    if let Err(e) = set_reconcile_status(client, &ns, &dc.name_any(), &next).await {
        log::warn!(
            "Could not record the reconcile status of {}/{}: {}",
            ns,
            dc.name_any(),
            e
        );
    }
}

/// Whether the stale-resource prune step is switched off.
///
/// Set `CICD_DISABLE_PRUNE=true` on the controller to make reconcile log what
/// it would have deleted instead of deleting it. Intended as a safety valve
/// during schema migrations, where a misread DeployConfig could otherwise
/// cause running workloads to be pruned. Everything else about reconcile
/// (applying manifests, annotations, owner references) is unchanged.
fn prune_disabled() -> bool {
    std::env::var("CICD_DISABLE_PRUNE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Error handler for the controller
fn error_policy(_dc: Arc<DeployConfig>, error: &AppError, _ctx: Arc<ControllerContext>) -> Action {
    log::error!(
        "Error during reconciliation:\n{}",
        format_error_chain(error)
    );
    Action::requeue(Duration::from_secs(5))
}

/// Start the Kubernetes controller
pub async fn start_controller(
    client: Client,
    _pool: Pool<SqliteConnectionManager>,
    // discord_notifier: Option<DiscordNotifier>,
) -> AppResult<()> {
    let context = Arc::new(ControllerContext {
        client: client.clone(),
        // discord_notifier,
    });

    // Create the API for DeployConfig resources
    let deploy_configs: Api<DeployConfig> = Api::all(client.clone());

    // Start the controller
    log::info!("Starting DeployConfig controller");

    // Create and start the controller
    Controller::new(deploy_configs, watcher::Config::default())
        .run(reconcile, error_policy, context.clone())
        .for_each(|res| async move {
            match res {
                Ok(o) => log::debug!("Reconciliation completed: {:?}", o),
                Err(e) => log::error!("Reconciliation error: {:?}", e),
            }
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::ErrorResponse;

    fn api_error(code: u16, message: &str) -> AppError {
        AppError::Kubernetes(kube::Error::Api(ErrorResponse {
            status: "Failure".into(),
            message: message.into(),
            reason: "Invalid".into(),
            code,
        }))
    }

    #[test]
    fn rejected_objects_are_permanent_and_the_rest_are_not() {
        assert!(is_permanent(&api_error(422, "invalid")));
        assert!(is_permanent(&api_error(403, "forbidden")));
        assert!(!is_permanent(&api_error(409, "conflict")));
        assert!(!is_permanent(&api_error(429, "slow down")));
        assert!(!is_permanent(&api_error(500, "etcd")));
        assert!(!is_permanent(&AppError::Internal("discovery".into())));
    }

    #[test]
    fn failures_name_every_resource() {
        let failures = vec![
            (
                "Deployment/postgres".to_string(),
                api_error(422, "spec.strategy.rollingUpdate: Forbidden"),
            ),
            ("Service/web".to_string(), api_error(500, "etcd timeout")),
        ];
        let f = Failure::from_applies(&failures).expect("a failure");
        assert_eq!(f.resources, vec!["Deployment/postgres", "Service/web"]);
        assert_eq!(
            f.message,
            "Deployment/postgres: spec.strategy.rollingUpdate: Forbidden (422)\n\
             Service/web: etcd timeout (500)"
        );
        assert!(f.permanent);
        assert!(Failure::from_applies(&[]).is_none());
    }

    #[test]
    fn a_continuing_failure_keeps_its_start_and_success_clears_it() {
        let failure = Failure {
            message: "Deployment/postgres: forbidden (422)".into(),
            resources: vec!["Deployment/postgres".into()],
            permanent: true,
        };
        let first = reconcile_status(None, Some("h1".into()), Some(&failure), "T1");
        assert_eq!(first.error.as_ref().map(|e| e.since.as_str()), Some("T1"));
        let again = reconcile_status(Some(&first), Some("h1".into()), Some(&failure), "T2");
        assert_eq!(again, first, "unchanged, so nothing is written");
        let cleared = reconcile_status(Some(&first), Some("h2".into()), None, "T3");
        assert_eq!(
            cleared,
            ReconcileStatus {
                render_hash: Some("h2".into()),
                error: None
            }
        );
    }
}

/// One reconcile against the real cluster, on a TestDeployConfig in the
/// `test-crd-rehearsal` namespace. Ignored by default:
///
/// ```text
/// cargo test --features test-crd -- --ignored live_reconcile
/// ```
#[cfg(all(test, feature = "test-crd"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod live {
    use super::*;
    use crate::kubernetes::cr_writers::{
        apply_config_sync, set_deployed_status, set_specs, DeployedStatus,
    };
    use crate::kubernetes::deploy_config::{DeployConfigSpecFields, Template};
    use crate::kubernetes::repo::{Repository, ShaMaybeBranch};
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::api::DeleteParams;
    use serde_json::json;

    const NS: &str = "test-crd-rehearsal";
    const NAME: &str = "reconcile-errors";

    fn deployment(strategy: serde_json::Value) -> serde_json::Value {
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "reconcile-errors-web" },
            "spec": {
                "replicas": 0,
                "strategy": strategy,
                "selector": { "matchLabels": { "app": "reconcile-errors-web" } },
                "template": {
                    "metadata": { "labels": { "app": "reconcile-errors-web" } },
                    "spec": { "containers": [{ "name": "pause", "image": "registry.k8s.io/pause:3.9" }] },
                },
            },
        })
    }

    async fn reconcile_once(client: &Client, specs: Vec<serde_json::Value>) -> DeployConfig {
        set_specs(client, NS, NAME, &specs, Some("c1"))
            .await
            .unwrap();
        let api: Api<DeployConfig> = Api::namespaced(client.clone(), NS);
        let dc = api.get(NAME).await.unwrap();
        let ctx = Arc::new(ControllerContext {
            client: client.clone(),
        });
        let _ = reconcile(Arc::new(dc), ctx).await;
        api.get(NAME).await.unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn live_reconcile_records_errors_and_applies_the_rest() {
        let client = Client::try_default().await.expect("kube client");
        let api: Api<DeployConfig> = Api::namespaced(client.clone(), NS);
        let _ = api.delete(NAME, &DeleteParams::default()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let fields = DeployConfigSpecFields {
            checks: Default::default(),
            team: "t".into(),
            kind: "service".into(),
            parameters: Default::default(),
            selections: Default::default(),
            patches: vec![],
            config: Repository {
                owner: "o".into(),
                repo: "c".into(),
            },
            specs: vec![],
        };
        apply_config_sync(&client, NS, NAME, &fields).await.unwrap();
        let status = DeployedStatus {
            parameters: Default::default(),
            config: Some(ShaMaybeBranch {
                sha: "c1".into(),
                branch: None,
            }),
        };
        set_deployed_status(&client, NS, NAME, &status)
            .await
            .unwrap();

        // A manifest the API server rejects, beside one it accepts.
        let config_map = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": "reconcile-errors-cm" },
            "data": { "a": "1" },
        });
        let invalid = deployment(json!({ "type": "Recreate", "rollingUpdate": { "maxSurge": 1 } }));
        let dc = reconcile_once(
            &client,
            vec![
                Template::stored("web.yaml", invalid),
                Template::stored("cm.yaml", config_map.clone()),
            ],
        )
        .await;
        let error = dc.reconcile_error().expect("the error is recorded").clone();
        assert_eq!(error.resources, vec!["Deployment/reconcile-errors-web"]);
        assert!(error.permanent);
        assert!(error.message.contains("422"), "{}", error.message);
        let children = dc.desired_children().unwrap();
        assert_eq!(
            dc.apply_state(Some(&children)),
            crate::kubernetes::deploy_config::ApplyState::Failing(error.clone())
        );
        // The ConfigMap after the failing Deployment was still applied.
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), NS);
        assert!(cms.get_opt("reconcile-errors-cm").await.unwrap().is_some());

        // Fixed: the error clears and the status says this render applied.
        let dc = reconcile_once(
            &client,
            vec![
                Template::stored("web.yaml", deployment(json!({ "type": "Recreate" }))),
                Template::stored("cm.yaml", config_map),
            ],
        )
        .await;
        assert!(dc.reconcile_error().is_none());
        let children = dc.desired_children().unwrap();
        assert_eq!(
            dc.apply_state(Some(&children)),
            crate::kubernetes::deploy_config::ApplyState::Applied
        );

        api.delete(NAME, &DeleteParams::default()).await.unwrap();
    }
}
