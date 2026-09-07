//! The writers to the DeployConfig custom resource, one field manager each.
//!
//! Every write is a server-side apply. A manager sends the complete set of
//! fields it owns every time, and the server removes any field it owned
//! but stopped sending. That is the whole contract, and it is why the
//! managers are split the way they are: a writer that sends only part of
//! what it owns would have the rest removed.
//!
//! | manager           | owns                                              |
//! |-------------------|---------------------------------------------------|
//! | cicd-config-sync  | spec.parameters, spec.config, spec.kind, spec.team; status.orphaned |
//! | cicd-deploy       | spec.specs; status.parameters, status.config      |
//! | cicd-selections   | spec.selections (the whole map)                   |
//! | cicd-patches      | spec.patches (the whole list)                     |
//! | cicd-autodeploy   | status.autodeploy                                 |
//!
//! Two rules learned from rehearsing this on a throwaway object:
//!
//! - To clear a map, omit the key. Applying `selections: {}` is rejected by
//!   schema validation (the server turns an owned, empty map into null);
//!   omitting `selections` removes every entry this manager owns.
//! - A field written by anyone else (a `kubectl patch`, or the merge-patch
//!   writers that predate this module) stays until that owner releases it.
//!   `kubernetes/migration/adopt-field-managers.sh` moves ownership of
//!   existing objects to these managers once; after that, removal works.
//!
//! Every apply uses force: our managers never share a field, so a conflict
//! can only be with a legacy or hand-made owner, and the writer is right.

use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::kubernetes::deploy_config::{DeployConfigSpecFields, DEPLOY_CONFIG_KIND};
use crate::kubernetes::parameters::ParameterValues;
use crate::kubernetes::patches::ManifestPatch;
use crate::kubernetes::repo::ShaMaybeBranch;
use crate::kubernetes::selections::Selections;
use crate::kubernetes::DeployConfig;

pub const API_VERSION: &str = "cicd.coolkev.com/v1";

pub mod manager {
    pub const CONFIG_SYNC: &str = "cicd-config-sync";
    pub const DEPLOY: &str = "cicd-deploy";
    pub const SELECTIONS: &str = "cicd-selections";
    pub const PATCHES: &str = "cicd-patches";
    pub const AUTODEPLOY: &str = "cicd-autodeploy";
}

/// The apply body: type meta, identity, and exactly the fields the manager
/// owns. `spec` and `status` are each either the complete owned set or
/// absent; an empty object under `spec` releases everything under it.
fn body(namespace: &str, name: &str, spec: Option<Value>, status: Option<Value>) -> Value {
    let mut body = json!({
        "apiVersion": API_VERSION,
        "kind": DEPLOY_CONFIG_KIND,
        "metadata": { "name": name, "namespace": namespace },
    });
    if let Some(spec) = spec {
        body["spec"] = spec;
    }
    if let Some(status) = status {
        body["status"] = status;
    }
    body
}

async fn apply(
    client: &Client,
    namespace: &str,
    manager: &str,
    body: &Value,
    subresource_status: bool,
) -> AppResult<()> {
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| AppError::Internal("apply body without a name".to_string()))?;
    let params = PatchParams::apply(manager).force();
    let patch = Patch::Apply(body);
    let result = if subresource_status {
        api.patch_status(name, &params, &patch).await
    } else {
        api.patch(name, &params, &patch).await
    };
    result.map(|_| ()).map_err(AppError::Kubernetes)
}

/// What config sync owns: the declaration from `.deploy/<name>.yaml`.
/// Selections, patches and specs are deliberately absent so they are
/// neither claimed nor touched. An empty parameter map is omitted, not
/// sent as `{}` (see the module doc); omission releases every entry.
pub fn config_sync_spec(fields: &DeployConfigSpecFields) -> Value {
    let mut spec = json!({
        "config": fields.config,
        "kind": fields.kind,
        "team": fields.team,
    });
    if !fields.parameters.is_empty() {
        spec["parameters"] = json!(fields.parameters);
    }
    spec
}

/// Create or update the declared part of a DeployConfig. Creates the object
/// when it does not exist.
pub async fn apply_config_sync(
    client: &Client,
    namespace: &str,
    name: &str,
    fields: &DeployConfigSpecFields,
) -> AppResult<()> {
    let body = body(namespace, name, Some(config_sync_spec(fields)), None);
    apply(client, namespace, manager::CONFIG_SYNC, &body, false).await
}

pub async fn set_orphaned(
    client: &Client,
    namespace: &str,
    name: &str,
    orphaned: bool,
) -> AppResult<()> {
    let body = body(namespace, name, None, Some(json!({ "orphaned": orphaned })));
    apply(client, namespace, manager::CONFIG_SYNC, &body, true).await
}

/// Replace the manifest templates. Owned by the deploy manager together
/// with the deployed status, since a deploy writes both.
pub async fn set_specs(
    client: &Client,
    namespace: &str,
    name: &str,
    specs: &[Value],
) -> AppResult<()> {
    let body = body(namespace, name, Some(json!({ "specs": specs })), None);
    apply(client, namespace, manager::DEPLOY, &body, false).await
}

/// What is deployed: every parameter's value and the config commit. Empty
/// means undeployed and releases both keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeployedStatus {
    pub parameters: ParameterValues,
    pub config: Option<ShaMaybeBranch>,
}

impl DeployedStatus {
    pub fn undeployed() -> Self {
        Self::default()
    }

    /// The `status` object the deploy manager applies. A pinned commit has
    /// no branch, and omitting it here is what removes the previous one.
    pub fn body(&self) -> Value {
        let mut status = json!({});
        if !self.parameters.is_empty() {
            status["parameters"] = json!(self.parameters);
        }
        if let Some(config) = &self.config {
            status["config"] = json!({ "sha": config.sha });
            if let Some(branch) = &config.branch {
                status["config"]["branch"] = json!(branch);
            }
        }
        status
    }
}

pub async fn set_deployed_status(
    client: &Client,
    namespace: &str,
    name: &str,
    status: &DeployedStatus,
) -> AppResult<()> {
    let body = body(namespace, name, None, Some(status.body()));
    apply(client, namespace, manager::DEPLOY, &body, true).await
}

/// The complete selection map. Empty omits the key, which releases every
/// entry (see the module doc for why not `{}`).
pub fn selections_spec(selections: &Selections) -> Value {
    if selections.is_empty() {
        json!({})
    } else {
        json!({ "selections": selections })
    }
}

pub async fn set_selections(
    client: &Client,
    namespace: &str,
    name: &str,
    selections: &Selections,
) -> AppResult<()> {
    let body = body(namespace, name, Some(selections_spec(selections)), None);
    apply(client, namespace, manager::SELECTIONS, &body, false).await
}

/// The complete patch list. Lists are atomic under server-side apply, so
/// an empty list is fine here.
pub async fn set_patches(
    client: &Client,
    namespace: &str,
    name: &str,
    patches: &[ManifestPatch],
) -> AppResult<()> {
    let body = body(namespace, name, Some(json!({ "patches": patches })), None);
    apply(client, namespace, manager::PATCHES, &body, false).await
}

pub async fn set_autodeploy(
    client: &Client,
    namespace: &str,
    name: &str,
    autodeploy: bool,
) -> AppResult<()> {
    let body = body(
        namespace,
        name,
        None,
        Some(json!({ "autodeploy": autodeploy })),
    );
    apply(client, namespace, manager::AUTODEPLOY, &body, true).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::parameters::{ParameterSource, ParameterValue, SHA_PARAMETER};
    use crate::kubernetes::repo::{Repository, RepositoryBranch};
    use crate::kubernetes::selections::{Durability, Selection};

    fn fields() -> DeployConfigSpecFields {
        let mut selections = Selections::default();
        selections.insert("SHA".into(), Selection::pin("abc", Durability::Standing));
        DeployConfigSpecFields {
            team: "t".into(),
            kind: "service".into(),
            parameters: ParameterSource::sha_map(Some(RepositoryBranch {
                owner: "o".into(),
                repo: "r".into(),
                branch: "master".into(),
            })),
            selections,
            patches: vec![],
            config: Repository {
                owner: "o".into(),
                repo: "c".into(),
            },
            specs: vec![json!({"kind": "Deployment"})],
        }
    }

    #[test]
    fn config_sync_sends_only_the_declaration() {
        let spec = config_sync_spec(&fields());
        assert_eq!(spec["parameters"]["SHA"]["type"], "commit");
        assert_eq!(spec["config"]["repo"], "c");
        assert_eq!(spec["kind"], "service");
        assert_eq!(spec["team"], "t");
        for owned_elsewhere in ["selections", "patches", "specs"] {
            assert!(
                spec.get(owned_elsewhere).is_none(),
                "{owned_elsewhere} must not be claimed"
            );
        }
        // No parameters at all omits the map; the server rejects an owned
        // empty map, and omission removes every entry anyway.
        let mut none = fields();
        none.parameters.clear();
        assert!(config_sync_spec(&none).get("parameters").is_none());
    }

    #[test]
    fn body_carries_type_meta_and_identity() {
        let b = body("ns", "n", Some(json!({"team": "t"})), None);
        assert_eq!(b["apiVersion"], API_VERSION);
        assert_eq!(b["kind"], DEPLOY_CONFIG_KIND);
        assert_eq!(b["metadata"]["namespace"], "ns");
        assert_eq!(b["spec"]["team"], "t");
        assert!(b.get("status").is_none());
    }

    #[test]
    fn deployed_status_omits_what_is_absent() {
        let mut parameters = ParameterValues::default();
        parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::Commit {
                value: "abc".into(),
                branch: None,
            },
        );
        parameters.insert(
            "GREETING".into(),
            ParameterValue::Value { value: "hi".into() },
        );
        let status = DeployedStatus {
            parameters,
            config: Some(ShaMaybeBranch {
                sha: "c1".into(),
                branch: Some("master".into()),
            }),
        }
        .body();
        assert_eq!(status["parameters"]["SHA"]["value"], "abc");
        assert!(
            status["parameters"]["SHA"].get("branch").is_none(),
            "a pin has no branch"
        );
        assert_eq!(status["parameters"]["GREETING"]["type"], "value");
        assert_eq!(status["config"]["branch"], "master");
        assert_eq!(DeployedStatus::undeployed().body(), json!({}));
    }

    #[test]
    fn empty_selections_omit_the_key() {
        assert_eq!(selections_spec(&Selections::default()), json!({}));
        let spec = selections_spec(&fields().selections);
        assert_eq!(spec["selections"]["SHA"]["pin"]["value"], "abc");
    }
}

/// Rehearsal against the real cluster, under the `test-crd` feature so it
/// writes TestDeployConfig objects. Ignored by default:
///
/// ```text
/// cargo test --features test-crd -- --ignored live_writers
/// ```
///
/// Exercises every writer in the order production uses them and asserts
/// that each manager owns only its keys and that omitted keys are removed.
#[cfg(all(test, feature = "test-crd"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod live {
    use super::*;
    use crate::kubernetes::parameters::{ParameterSource, ParameterValue, SHA_PARAMETER};
    use crate::kubernetes::patches::{PatchOp, PatchTarget};
    use crate::kubernetes::repo::{Repository, RepositoryBranch};
    use crate::kubernetes::selections::{Durability, Selection};
    use kube::api::DeleteParams;
    use std::collections::BTreeSet;

    const NS: &str = "test-crd-rehearsal";
    const NAME: &str = "writers";

    async fn get(client: &Client) -> Value {
        let api: Api<DeployConfig> = Api::namespaced(client.clone(), NS);
        let obj = api.get(NAME).await.expect("object exists");
        serde_json::to_value(&obj).unwrap()
    }

    async fn managers(client: &Client) -> BTreeSet<(String, bool)> {
        let api: Api<DeployConfig> = Api::namespaced(client.clone(), NS);
        let obj = api.get(NAME).await.expect("object exists");
        obj.metadata
            .managed_fields
            .unwrap_or_default()
            .into_iter()
            .map(|m| (m.manager.unwrap_or_default(), m.subresource.is_some()))
            .collect()
    }

    fn fields(with_old: bool) -> DeployConfigSpecFields {
        let mut parameters = ParameterSource::sha_map(Some(RepositoryBranch {
            owner: "o".into(),
            repo: "r".into(),
            branch: "master".into(),
        }));
        if with_old {
            parameters.insert(
                "OLD".into(),
                ParameterSource::Value {
                    default: "x".into(),
                },
            );
        }
        DeployConfigSpecFields {
            team: "t".into(),
            kind: "service".into(),
            parameters,
            selections: Default::default(),
            patches: vec![],
            config: Repository {
                owner: "o".into(),
                repo: "c".into(),
            },
            specs: vec![],
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_writers_own_only_their_keys_and_remove_what_they_omit() {
        let client = Client::try_default().await.expect("kube client");
        let api: Api<DeployConfig> = Api::namespaced(client.clone(), NS);
        let _ = api.delete(NAME, &DeleteParams::default()).await;

        // 1. Config sync creates the object.
        apply_config_sync(&client, NS, NAME, &fields(true))
            .await
            .unwrap();
        set_orphaned(&client, NS, NAME, false).await.unwrap();
        let obj = get(&client).await;
        assert_eq!(obj["spec"]["parameters"]["OLD"]["default"], "x");
        assert_eq!(obj["status"]["orphaned"], false);

        // 2. A deploy: specs + status; then intent: selections, patches, autodeploy.
        set_specs(
            &client,
            NS,
            NAME,
            &[json!({"file": "d.yaml", "manifest": {"kind": "Deployment"}})],
        )
        .await
        .unwrap();
        let mut parameters = ParameterValues::default();
        parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::Commit {
                value: "aaa".into(),
                branch: Some("feat".into()),
            },
        );
        parameters.insert("OLD".into(), ParameterValue::Value { value: "x".into() });
        set_deployed_status(
            &client,
            NS,
            NAME,
            &DeployedStatus {
                parameters,
                config: Some(ShaMaybeBranch {
                    sha: "ccc".into(),
                    branch: Some("master".into()),
                }),
            },
        )
        .await
        .unwrap();
        let mut selections = Selections::default();
        selections.insert(
            SHA_PARAMETER.into(),
            Selection::track("feat", Durability::Temporary),
        );
        selections.insert("OLD".into(), Selection::pin("y", Durability::Standing));
        set_selections(&client, NS, NAME, &selections)
            .await
            .unwrap();
        let patch = ManifestPatch {
            target: PatchTarget {
                file: None,
                kind: "Deployment".into(),
                name: "w".into(),
            },
            op: PatchOp::Replace,
            path: "/spec/replicas".into(),
            value: Some(json!(2)),
            durability: Durability::Temporary,
            note: None,
            by: None,
            since: None,
        };
        set_patches(&client, NS, NAME, &[patch]).await.unwrap();
        set_autodeploy(&client, NS, NAME, true).await.unwrap();

        let obj = get(&client).await;
        assert_eq!(obj["spec"]["specs"].as_array().unwrap().len(), 1);
        assert_eq!(obj["spec"]["selections"]["OLD"]["pin"]["value"], "y");
        assert_eq!(obj["spec"]["patches"].as_array().unwrap().len(), 1);
        assert_eq!(obj["status"]["parameters"]["SHA"]["branch"], "feat");
        assert_eq!(obj["status"]["autodeploy"], true);
        let expected: BTreeSet<(String, bool)> = [
            (manager::CONFIG_SYNC, false),
            (manager::CONFIG_SYNC, true),
            (manager::DEPLOY, false),
            (manager::DEPLOY, true),
            (manager::SELECTIONS, false),
            (manager::PATCHES, false),
            (manager::AUTODEPLOY, true),
        ]
        .into_iter()
        .map(|(m, s)| (m.to_string(), s))
        .collect();
        assert_eq!(
            managers(&client).await,
            expected,
            "exactly our managers, no legacy owner"
        );

        // 3. The repo drops OLD: config sync omits it and it goes away,
        //    while everything owned elsewhere stays.
        apply_config_sync(&client, NS, NAME, &fields(false))
            .await
            .unwrap();
        let obj = get(&client).await;
        assert!(
            obj["spec"]["parameters"].get("OLD").is_none(),
            "OLD removed by omission"
        );
        assert_eq!(obj["spec"]["selections"]["OLD"]["pin"]["value"], "y");
        assert_eq!(obj["status"]["parameters"]["OLD"]["value"], "x");

        // 4. A pinned redeploy: no branch, no OLD; autodeploy and orphaned untouched.
        let mut parameters = ParameterValues::default();
        parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::Commit {
                value: "bbb".into(),
                branch: None,
            },
        );
        set_deployed_status(
            &client,
            NS,
            NAME,
            &DeployedStatus {
                parameters,
                config: Some(ShaMaybeBranch {
                    sha: "ddd".into(),
                    branch: None,
                }),
            },
        )
        .await
        .unwrap();
        let obj = get(&client).await;
        assert!(obj["status"]["parameters"]["SHA"].get("branch").is_none());
        assert!(obj["status"]["parameters"].get("OLD").is_none());
        assert!(obj["status"]["config"].get("branch").is_none());
        assert_eq!(obj["status"]["autodeploy"], true);
        assert_eq!(obj["status"]["orphaned"], false);

        // 5. Clearing intent.
        set_selections(&client, NS, NAME, &Selections::default())
            .await
            .unwrap();
        set_patches(&client, NS, NAME, &[]).await.unwrap();
        let obj = get(&client).await;
        assert!(
            obj["spec"].get("selections").is_none(),
            "selections gone by omission"
        );
        // Serialized through the Rust type, which skips an empty list.
        assert!(obj["spec"]["patches"]
            .as_array()
            .is_none_or(|a| a.is_empty()));

        // 6. Undeploy, then orphan.
        set_specs(&client, NS, NAME, &[]).await.unwrap();
        set_deployed_status(&client, NS, NAME, &DeployedStatus::undeployed())
            .await
            .unwrap();
        let obj = get(&client).await;
        assert!(obj["status"].get("parameters").is_none());
        assert!(obj["status"].get("config").is_none());
        assert_eq!(obj["status"]["autodeploy"], true);
        set_orphaned(&client, NS, NAME, true).await.unwrap();
        let obj = get(&client).await;
        assert_eq!(obj["status"]["orphaned"], true);
        assert_eq!(obj["status"]["autodeploy"], true);

        // 7. A config-only config: no parameters at all.
        let mut bare = fields(false);
        bare.parameters.clear();
        apply_config_sync(&client, NS, NAME, &bare).await.unwrap();
        let obj = get(&client).await;
        assert!(
            obj["spec"].get("parameters").is_none(),
            "parameters gone by omission"
        );
        assert_eq!(obj["spec"]["team"], "t");

        api.delete(NAME, &DeleteParams::default()).await.unwrap();
    }
}
