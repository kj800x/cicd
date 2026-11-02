//! Sending the API server's update-strategy defaults explicitly, so the
//! controller owns them.
//!
//! A Deployment applied without a strategy gets `type: RollingUpdate` and a
//! `rollingUpdate` block from the API server's defaulting, and defaulted
//! fields belong to no field manager. When a later manifest says
//! `strategy: {type: Recreate}`, the apply sets the type but cannot remove a
//! `rollingUpdate` it never owned, and validation rejects the result:
//!
//! ```text
//! 422 spec.strategy.rollingUpdate: Forbidden: may not be specified when
//!     strategy type is 'Recreate'
//! ```
//!
//! StatefulSets switching to `updateStrategy.type: OnDelete` are rejected
//! the same way. DaemonSets switching to `OnDelete` are accepted but keep
//! the stale block as dead config.
//!
//! So every render fills in the defaults the API server would add, field by
//! field, and the apply sends them. They equal what the server stores, so
//! sending them changes nothing on the object; it only makes
//! `cicd-controller` their owner. A manifest that later switches to a
//! non-rolling type no longer renders them, and server-side apply removes
//! them like any other field we stopped sending. Everything stays a plain
//! server-side apply.
//!
//! The values mirror the API server's defaults (checked against this cluster,
//! k3s v1.31, 2026-11-01). If a release adds a defaulted field here (the
//! StatefulSet `maxUnavailable` behind the MaxUnavailableStatefulSet gate,
//! for example), it has to be added below, or that field is unowned again.
use serde_json::{json, Map, Value};

/// One kind's strategy field and its defaults.
struct Defaults {
    kind: &'static str,
    /// Field under `spec` holding the strategy.
    field: &'static str,
    /// Defaulted members of `rollingUpdate` when the type is RollingUpdate.
    rolling_update: fn() -> Value,
}

const ROLLING_UPDATE: &str = "RollingUpdate";

const DEFAULTS: &[Defaults] = &[
    Defaults {
        kind: "Deployment",
        field: "strategy",
        rolling_update: || json!({ "maxSurge": "25%", "maxUnavailable": "25%" }),
    },
    Defaults {
        kind: "DaemonSet",
        field: "updateStrategy",
        rolling_update: || json!({ "maxSurge": 0, "maxUnavailable": 1 }),
    },
    Defaults {
        kind: "StatefulSet",
        field: "updateStrategy",
        rolling_update: || json!({ "partition": 0 }),
    },
];

/// Fill in the update-strategy defaults of an `apps/v1` Deployment,
/// DaemonSet or StatefulSet manifest, leaving every value the manifest sets
/// alone. Other manifests, and ones whose shape is not what the API would
/// accept anyway, are left untouched.
pub fn own_strategy_defaults(manifest: &mut Value) {
    if manifest["apiVersion"].as_str() != Some("apps/v1") {
        return;
    }
    let Some(defaults) = manifest["kind"]
        .as_str()
        .and_then(|kind| DEFAULTS.iter().find(|d| d.kind == kind))
    else {
        return;
    };
    let Some(spec) = manifest.get_mut("spec").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(strategy) = object_entry(spec, defaults.field) else {
        return;
    };
    let kind = strategy
        .entry("type")
        .or_insert_with(|| json!(ROLLING_UPDATE));
    if kind.as_str() != Some(ROLLING_UPDATE) {
        return;
    }
    let Some(rolling_update) = object_entry(strategy, "rollingUpdate") else {
        return;
    };
    if let Value::Object(fields) = (defaults.rolling_update)() {
        for (key, value) in fields {
            rolling_update.entry(key).or_insert(value);
        }
    }
}

/// The object at `key`, created empty when missing; `None` when something
/// other than an object (or null) is there.
fn object_entry<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Map<String, Value>> {
    let entry = map.entry(key).or_insert_with(|| json!({}));
    if entry.is_null() {
        *entry = json!({});
    }
    entry.as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(kind: &str, strategy: Option<Value>) -> Value {
        let field = if kind == "Deployment" {
            "strategy"
        } else {
            "updateStrategy"
        };
        let mut m = json!({
            "apiVersion": "apps/v1",
            "kind": kind,
            "metadata": { "name": "x" },
            "spec": { "template": {} },
        });
        if let Some(s) = strategy {
            m["spec"][field] = s;
        }
        m
    }

    fn filled(mut m: Value) -> Value {
        own_strategy_defaults(&mut m);
        m
    }

    #[test]
    fn a_manifest_without_a_strategy_gets_the_server_defaults() {
        assert_eq!(
            filled(manifest("Deployment", None))["spec"]["strategy"],
            json!({ "type": "RollingUpdate", "rollingUpdate": { "maxSurge": "25%", "maxUnavailable": "25%" } })
        );
        assert_eq!(
            filled(manifest("DaemonSet", None))["spec"]["updateStrategy"],
            json!({ "type": "RollingUpdate", "rollingUpdate": { "maxSurge": 0, "maxUnavailable": 1 } })
        );
        assert_eq!(
            filled(manifest("StatefulSet", None))["spec"]["updateStrategy"],
            json!({ "type": "RollingUpdate", "rollingUpdate": { "partition": 0 } })
        );
    }

    #[test]
    fn values_the_manifest_sets_are_kept_and_only_the_rest_filled() {
        let m = manifest(
            "Deployment",
            Some(json!({ "rollingUpdate": { "maxUnavailable": 1 } })),
        );
        assert_eq!(
            filled(m)["spec"]["strategy"],
            json!({ "type": "RollingUpdate", "rollingUpdate": { "maxSurge": "25%", "maxUnavailable": 1 } })
        );
        let m = manifest(
            "Deployment",
            Some(json!({ "type": "RollingUpdate", "rollingUpdate": null })),
        );
        assert_eq!(
            filled(m)["spec"]["strategy"]["rollingUpdate"],
            json!({ "maxSurge": "25%", "maxUnavailable": "25%" })
        );
    }

    #[test]
    fn non_rolling_types_get_no_rolling_update_block() {
        let m = manifest("Deployment", Some(json!({ "type": "Recreate" })));
        assert_eq!(filled(m)["spec"]["strategy"], json!({ "type": "Recreate" }));
        for kind in ["DaemonSet", "StatefulSet"] {
            let m = manifest(kind, Some(json!({ "type": "OnDelete" })));
            assert_eq!(
                filled(m)["spec"]["updateStrategy"],
                json!({ "type": "OnDelete" })
            );
        }
    }

    #[test]
    fn other_manifests_are_untouched() {
        for m in [
            json!({ "apiVersion": "v1", "kind": "Service", "spec": {} }),
            json!({ "apiVersion": "extensions/v1beta1", "kind": "Deployment", "spec": {} }),
            json!({ "apiVersion": "apps/v1", "kind": "ReplicaSet", "spec": {} }),
            json!({ "apiVersion": "apps/v1", "kind": "Deployment" }),
            json!({ "apiVersion": "apps/v1", "kind": "Deployment", "spec": { "strategy": "bad" } }),
        ] {
            assert_eq!(filled(m.clone()), m);
        }
    }
}

/// Against the real cluster, in the `test-crd-rehearsal` namespace. Ignored
/// by default:
///
/// ```text
/// cargo test -- --ignored live_strategy
/// ```
///
/// For each kind: an object created the way existing objects were (defaults
/// unowned) reproduces the problem; one apply with the defaults filled in
/// takes them over without changing the object; then switching to the
/// non-rolling type and back is plain server-side apply, with no new
/// ReplicaSet or controller revision.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod live {
    use super::own_strategy_defaults;
    use crate::kubernetes::apply;
    use kube::api::{Api, DeleteParams, DynamicObject, ListParams, Patch, PatchParams};
    use kube::Client;
    use serde_json::{json, Value};

    const NS: &str = "test-crd-rehearsal";
    const MANAGER: &str = "cicd-controller";

    fn workload(kind: &str, name: &str, strategy: Option<Value>) -> Value {
        let mut spec = json!({
            "selector": { "matchLabels": { "app": name } },
            "template": {
                "metadata": { "labels": { "app": name } },
                "spec": {
                    "terminationGracePeriodSeconds": 0,
                    "containers": [{ "name": "pause", "image": "registry.k8s.io/pause:3.9" }],
                },
            },
        });
        let field = if kind == "Deployment" {
            "strategy"
        } else {
            "updateStrategy"
        };
        match kind {
            "Deployment" => spec["replicas"] = json!(1),
            "StatefulSet" => {
                spec["replicas"] = json!(1);
                spec["serviceName"] = json!(name);
            }
            // No node matches, so the DaemonSet runs nothing.
            _ => spec["template"]["spec"]["nodeSelector"] = json!({ "cicd-test": "none" }),
        }
        if let Some(s) = strategy {
            spec[field] = s;
        }
        json!({
            "apiVersion": "apps/v1",
            "kind": kind,
            "metadata": { "name": name, "namespace": NS },
            "spec": spec,
        })
    }

    /// The manifest as the controller now renders it.
    fn rendered(manifest: Value) -> DynamicObject {
        let mut manifest = manifest;
        own_strategy_defaults(&mut manifest);
        serde_json::from_value(manifest).unwrap()
    }

    fn api(client: &Client, kind: &str) -> Api<DynamicObject> {
        let ar =
            kube::api::ApiResource::from_gvk(&kube::api::GroupVersionKind::gvk("apps", "v1", kind));
        Api::namespaced_with(client.clone(), NS, &ar)
    }

    /// ReplicaSets or ControllerRevisions behind `name`: a new one would
    /// mean the change rolled the pods.
    async fn revisions(client: &Client, kind: &str, name: &str) -> Vec<String> {
        let child = if kind == "Deployment" {
            "ReplicaSet"
        } else {
            "ControllerRevision"
        };
        let list = api(client, child)
            .list(&ListParams::default().labels(&format!("app={name}")))
            .await
            .unwrap();
        let mut names: Vec<String> = list
            .items
            .into_iter()
            .filter_map(|o| o.metadata.name)
            .collect();
        names.sort();
        names
    }

    async fn live(api: &Api<DynamicObject>, name: &str) -> Value {
        serde_json::to_value(api.get(name).await.unwrap()).unwrap()
    }

    async fn check(kind: &str, name: &str, field: &str, target: &str) {
        let client = Client::try_default().await.expect("kube client");
        let api = api(&client, kind);
        let _ = api.delete(name, &DeleteParams::foreground()).await;
        for _ in 0..30 {
            if api.get_opt(name).await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let ssa = PatchParams::apply(MANAGER).force();
        let target_strategy = json!({ "type": target });

        // An existing object: applied before this change, defaults unowned.
        api.patch(name, &ssa, &Patch::Apply(workload(kind, name, None)))
            .await
            .unwrap();
        let before = live(&api, name).await;
        assert!(
            before["spec"][field].get("rollingUpdate").is_some(),
            "{kind} was defaulted"
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let revisions_before = revisions(&client, kind, name).await;

        // The first reconcile after release takes the defaults over and
        // changes nothing else: same generation, same strategy.
        apply(&client, NS, rendered(workload(kind, name, None)))
            .await
            .unwrap();
        let claimed = live(&api, name).await;
        assert_eq!(
            claimed["metadata"]["generation"], before["metadata"]["generation"],
            "{kind}"
        );
        assert_eq!(claimed["spec"][field], before["spec"][field], "{kind}");

        // The switch is now a plain apply.
        apply(
            &client,
            NS,
            rendered(workload(kind, name, Some(target_strategy.clone()))),
        )
        .await
        .unwrap();
        assert_eq!(
            live(&api, name).await["spec"][field],
            target_strategy,
            "{kind}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert_eq!(
            revisions(&client, kind, name).await,
            revisions_before,
            "{kind} did not roll"
        );

        // And back.
        apply(&client, NS, rendered(workload(kind, name, None)))
            .await
            .unwrap();
        assert_eq!(
            live(&api, name).await["spec"][field],
            before["spec"][field],
            "{kind}"
        );
        assert_eq!(
            revisions(&client, kind, name).await,
            revisions_before,
            "{kind} did not roll back"
        );

        api.delete(name, &DeleteParams::foreground()).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn live_strategy_deployment_to_recreate() {
        check(
            "Deployment",
            "strategy-defaults-deploy",
            "strategy",
            "Recreate",
        )
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn live_strategy_daemonset_to_on_delete() {
        check(
            "DaemonSet",
            "strategy-defaults-ds",
            "updateStrategy",
            "OnDelete",
        )
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn live_strategy_statefulset_to_on_delete() {
        check(
            "StatefulSet",
            "strategy-defaults-sts",
            "updateStrategy",
            "OnDelete",
        )
        .await;
    }
}
