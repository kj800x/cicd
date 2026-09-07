use std::collections::BTreeMap;

use crate::kubernetes::{
    parameters::{
        ParameterSource, ParameterSources, ParameterValue, ParameterValues, SHA_PARAMETER,
    },
    patches::{apply_patches, ManifestPatch},
    repo::{DeploymentState, RepositoryBranch, ShaMaybeBranch},
    selections::{Durability, Mode, Selection, Selections},
    Repository,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{api::DynamicObject, CustomResource, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEPLOY_CONFIG_KIND: &str = if cfg!(feature = "test-crd") {
    "TestDeployConfig"
} else {
    "DeployConfig"
};

/// DeployConfig status information
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployConfigStatus {
    /// The value currently deployed for each named parameter.
    /// The legacy artifact lives under [`SHA_PARAMETER`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: ParameterValues,

    /// Information about the current state of the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ShaMaybeBranch>,

    /// The current state of autodeploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autodeploy: Option<bool>,

    /// Whether the deploy config is orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned: Option<bool>,
}

/// DeployConfig spec fields represent the desired state for a deployment
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeployConfigSpecFields {
    /// Team
    pub team: String,

    /// Kind of deployable.
    /// Typed as a string to allow for future flexibility.
    /// Right now valid values are "service", "worker", "job", "meta", etc.
    pub kind: String,

    /// Named parameters. The legacy artifact repo lives under
    /// [`SHA_PARAMETER`], whose value is substituted for `$SHA` in specs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: ParameterSources,

    /// What each parameter is following: an override channel, a pin, or
    /// (absent) the source's default channel. Written by the deploy handler
    /// only; config sync never touches it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selections: Selections,

    /// Ad hoc JSON Patch operations applied to the rendered manifests, in
    /// order. Written by the deploy handler only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<ManifestPatch>,

    /// Repository information
    pub config: Repository,

    /// Manifest templates as they were in the config repo at the deployed
    /// config commit. Each entry is `{file, manifest}`; entries written
    /// before filenames were recorded are the bare manifest and are read
    /// as having no file. Written by the deploy handler only.
    #[serde(default)]
    pub specs: Vec<serde_json::Value>,
}

/// One manifest template and the `.deploy/<config>/` file it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub file: Option<String>,
    pub manifest: serde_json::Value,
}

impl Template {
    /// The stored form: `{"file": ..., "manifest": ...}`.
    pub fn stored(file: &str, manifest: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "file": file, "manifest": manifest })
    }

    /// Read either the stored form or a bare manifest from before filenames
    /// were recorded. A bare manifest always has `kind`, and the stored form
    /// never does at the top level, so the two cannot be confused.
    pub fn from_stored(value: &serde_json::Value) -> Template {
        match (value.get("file"), value.get("manifest")) {
            (Some(file), Some(manifest)) if value.get("kind").is_none() => Template {
                file: file.as_str().map(String::from),
                manifest: manifest.clone(),
            },
            _ => Template {
                file: None,
                manifest: value.clone(),
            },
        }
    }
}

/// The DeployConfig CustomResource
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(
    feature = "test-crd",
    kube(kind = "TestDeployConfig", shortname = "tdc")
)]
#[cfg_attr(
    not(feature = "test-crd"),
    kube(kind = "DeployConfig", shortname = "dc")
)]
#[kube(
    group = "cicd.coolkev.com",
    version = "v1",
    namespaced,
    schema = "disabled",
    status = "DeployConfigStatus",
    printcolumn = r#"{"name":"Team", "jsonPath":".spec.team", "type": "string"}"#,
    printcolumn = r#"{"name":"Kind", "jsonPath":".spec.kind", "type": "string"}"#,
    printcolumn = r#"{"name":"Artifact Repo", "jsonPath":".spec.parameters.SHA.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Config Repo", "jsonPath":".spec.config.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Config SHA", "jsonPath":".status.config.sha", "type":"string"}"#,
    printcolumn = r#"{"name":"Artifact SHA", "jsonPath":".status.parameters.SHA.value", "type":"string"}"#,
    printcolumn = r#"{"name":"Autodeploy", "jsonPath":".status.autodeploy", "type":"boolean"}"#,
    printcolumn = r#"{"name":"Age", "jsonPath":".metadata.creationTimestamp", "type":"date"}"#,
    printcolumn = r#"{"name":"Orphaned", "jsonPath":".status.orphaned", "type":"boolean"}"#
)]
pub struct DeployConfigSpec {
    /// Repository information and resource spec
    #[serde(flatten)]
    pub spec: DeployConfigSpecFields,
}

#[cfg(feature = "test-crd")]
pub type DeployConfig = TestDeployConfig;

impl DeployConfig {
    pub fn autodeploy(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.autodeploy)
            .unwrap_or(false)
    }

    pub fn is_orphaned(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.orphaned)
            .unwrap_or(false)
    }

    /// Whether this config depends on `repo`, either as the config repo
    /// (where its `.deploy/` manifests live) or as the artifact repo (where
    /// its image is built). GitHub owner and repo names are
    /// case-insensitive, so the comparison is too.
    pub fn references_repo(&self, repo: &Repository) -> bool {
        let same = |owner: &str, name: &str| {
            owner.eq_ignore_ascii_case(&repo.owner) && name.eq_ignore_ascii_case(&repo.repo)
        };

        let config = &self.spec.spec.config;
        if same(&config.owner, &config.repo) {
            return true;
        }

        self.spec
            .spec
            .parameters
            .values()
            .filter_map(ParameterSource::as_repository_branch)
            .any(|source| same(&source.owner, &source.repo))
    }

    /// The source of the [`SHA_PARAMETER`] parameter.
    fn sha_source(&self) -> Option<RepositoryBranch> {
        self.spec
            .spec
            .parameters
            .get(SHA_PARAMETER)
            .and_then(ParameterSource::as_repository_branch)
    }

    /// The deployed value of the [`SHA_PARAMETER`] parameter.
    fn sha_value(&self) -> Option<ShaMaybeBranch> {
        self.status
            .as_ref()?
            .parameters
            .get(SHA_PARAMETER)
            .and_then(ParameterValue::as_sha_maybe_branch)
    }

    pub fn supports_bounce(&self) -> bool {
        self.resource_specs().iter().any(|spec| {
            spec.get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                == "Deployment"
        })
    }

    pub fn supports_execute_job(&self) -> bool {
        self.resource_specs().iter().any(|spec| {
            spec.get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                == "CronJob"
        })
    }

    pub fn deployment_state(&self) -> DeploymentState {
        if let Some(config) = self.status.as_ref().and_then(|s| s.config.as_ref()) {
            if let Some(artifact) = self.sha_value() {
                DeploymentState::DeployedWithArtifact {
                    artifact,
                    config: config.clone(),
                }
            } else {
                DeploymentState::DeployedOnlyConfig {
                    config: config.clone(),
                }
            }
        } else {
            DeploymentState::Undeployed
        }
    }

    /// The effective selection for a parameter: the explicit one under
    /// `spec.selections`, or one derived from what is deployed.
    ///
    /// The derivation is the rule the code always used implicitly: a
    /// deployed value with no branch is a pin, a branch other than the
    /// source's default is an override, the default branch is the default.
    /// Derived overrides default to temporary for branches and standing for
    /// pins, matching what the UI proposes when making them explicit.
    pub fn selection(&self, parameter: &str) -> Selection {
        if let Some(explicit) = self.spec.spec.selections.get(parameter) {
            return explicit.clone();
        }
        if parameter != SHA_PARAMETER {
            return Selection::default();
        }
        let default_branch = self.artifact_repository().map(|r| r.branch);
        match self.sha_value() {
            Some(ShaMaybeBranch { sha, branch: None }) => {
                Selection::pin(&sha, Durability::Standing)
            }
            Some(ShaMaybeBranch {
                branch: Some(branch),
                ..
            }) if Some(branch.as_str()) != default_branch.as_deref() => {
                Selection::track(&branch, Durability::Temporary)
            }
            _ => Selection::default(),
        }
    }

    /// A temporary deployment has at least one temporary override active.
    /// It is what the badge, the homepage list and autodeploy's suspension
    /// key on. Standing overrides (a pinned dependency, a replica bump) are
    /// ordinary operation and do not count.
    #[allow(dead_code)] // the deploy page and env vars use this in the next PRs
    pub fn is_temporary_deployment(&self) -> bool {
        if matches!(self.deployment_state(), DeploymentState::Undeployed) {
            return false;
        }
        let mut names: Vec<&str> = self
            .spec
            .spec
            .selections
            .keys()
            .map(String::as_str)
            .collect();
        if !names.contains(&SHA_PARAMETER) {
            names.push(SHA_PARAMETER);
        }
        names.into_iter().any(|n| self.selection(n).is_temporary())
            || self
                .spec
                .spec
                .patches
                .iter()
                .any(ManifestPatch::is_temporary)
    }

    /// The `CICD_*` environment variables to inject into every container of the
    /// deployed workloads. Describes the deploy so apps can report their version
    /// and make deploy-aware decisions.
    ///
    /// - `CICD_DEPLOY_CONFIG`, `CICD_TEAM`
    /// - `CICD_TEMPORARY_DEPLOY`: `true` while any temporary override is
    ///   active. The signal for refusing dangerous work such as schema
    ///   migrations; a standing pin does not trip it.
    /// - `CICD_PARAM_<NAME>`: each parameter's deployed value, plus
    ///   `CICD_PARAM_<NAME>_MODE` (`track`, `override`, `pin`) and, unless
    ///   pinned, `CICD_PARAM_<NAME>_CHANNEL` (the branch being followed).
    /// - `CICD_CONFIG_SHA`, `CICD_CONFIG_BRANCH`
    pub fn deploy_env_vars(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = vec![
            ("CICD_DEPLOY_CONFIG".to_string(), self.name_any()),
            ("CICD_TEAM".to_string(), self.team().to_string()),
            (
                "CICD_TEMPORARY_DEPLOY".to_string(),
                self.is_temporary_deployment().to_string(),
            ),
        ];

        let Some(status) = self.status.as_ref() else {
            return vars;
        };

        for (name, value) in &status.parameters {
            let key = name
                .to_ascii_uppercase()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
            let selection = self.selection(name);
            let (mode, channel) = match selection.mode() {
                Mode::Pin(_) => ("pin", None),
                Mode::Track(branch) => ("override", Some(branch.to_string())),
                Mode::Default => ("track", value.branch().map(String::from)),
            };
            vars.push((format!("CICD_PARAM_{key}"), value.rendered()));
            vars.push((format!("CICD_PARAM_{key}_MODE"), mode.to_string()));
            if let Some(channel) = channel {
                vars.push((format!("CICD_PARAM_{key}_CHANNEL"), channel));
            }
        }

        if let Some(config) = &status.config {
            vars.push(("CICD_CONFIG_SHA".to_string(), config.sha.clone()));
            vars.push((
                "CICD_CONFIG_BRANCH".to_string(),
                config.branch.clone().unwrap_or_default(),
            ));
        }

        vars
    }

    /// Returns the owner reference to be applied to child resources
    pub fn child_owner_reference(&self) -> OwnerReference {
        OwnerReference {
            api_version: String::from("cicd.coolkev.com/v1"),
            kind: String::from(DEPLOY_CONFIG_KIND),
            name: self.name_any(),
            #[allow(clippy::expect_used)]
            uid: self.uid().expect("DeployConfig should have a UID"),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    /// Get the team name
    pub fn team(&self) -> &str {
        &self.spec.spec.team
    }

    /// Get the kind
    pub fn kind(&self) -> &str {
        &self.spec.spec.kind
    }

    /// Get a RepositoryBranch struct for the artifact (the [`SHA_PARAMETER`] parameter)
    pub fn artifact_repository(&self) -> Option<RepositoryBranch> {
        self.sha_source()
    }

    /// Get a Repository struct for the config
    pub fn config_repository(&self) -> Repository {
        self.spec.spec.config.clone()
    }

    /// The manifest templates, without their filenames.
    pub fn resource_specs(&self) -> Vec<serde_json::Value> {
        self.resource_templates()
            .into_iter()
            .map(|t| t.manifest)
            .collect()
    }

    /// The manifest templates with the file each came from, when known.
    pub fn resource_templates(&self) -> Vec<Template> {
        self.spec
            .spec
            .specs
            .iter()
            .map(Template::from_stored)
            .collect()
    }

    /// What every static (`value`) parameter resolves to right now: its
    /// pinned value if the selection pins it, otherwise its default.
    pub fn resolve_value_parameters(&self) -> ParameterValues {
        self.spec
            .spec
            .parameters
            .iter()
            .filter_map(|(name, source)| {
                let default = source.default_value()?;
                let value = match self.selection(name).mode() {
                    crate::kubernetes::selections::Mode::Pin(v) => v.to_string(),
                    _ => default.to_string(),
                };
                Some((name.clone(), ParameterValue::Value { value }))
            })
            .collect()
    }

    /// The rendered value of every deployed parameter, keyed by name, as it
    /// is substituted for `$NAME` in the manifests.
    pub fn parameter_values(&self) -> BTreeMap<String, String> {
        self.status
            .as_ref()
            .map(|s| {
                s.parameters
                    .iter()
                    .map(|(name, value)| (name.clone(), value.rendered()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The manifests with every parameter substituted, or an error naming
    /// the first declared parameter a template mentions that has no
    /// deployed value. Undeclared `$TOKENS` are left alone; they belong to
    /// the shell or the kubelet, not to us.
    ///
    /// This is the second half of the rule from the design: deployed means
    /// every declared parameter has a value, and anything else is refused
    /// rather than rendered with a literal `$NAME`.
    pub fn render_manifests(&self) -> crate::error::AppResult<Vec<serde_json::Value>> {
        use crate::kubernetes::spec_editing::{referenced_parameters, WithParameters};
        let values = self.parameter_values();
        let declared = &self.spec.spec.parameters;
        let templates = self.resource_templates();
        let mut rendered = Vec::with_capacity(templates.len());
        for template in &templates {
            for name in referenced_parameters(&template.manifest) {
                if declared.contains_key(&name) && !values.contains_key(&name) {
                    return Err(crate::error::AppError::Internal(format!(
                        "DeployConfig {} declares parameter {name} and its manifests use ${name}, \
                         but no value is deployed for it; refusing to render",
                        self.name_any()
                    )));
                }
            }
            rendered.push(template.manifest.with_parameters(&values));
        }
        // Patches apply after substitution, and a patch that no longer fits
        // fails the whole render rather than being skipped.
        apply_patches(&templates, rendered, &self.spec.spec.patches)
    }

    /// Turn rendered manifests into the child objects the controller applies:
    /// parsed, moved into the test-mode namespace when that is on, with the
    /// deploy env vars injected and this config's owner reference, labels and
    /// annotations set. Manifest kinds test mode skips are dropped.
    ///
    /// The controller and patch validation both go through here so a dry run
    /// sees exactly the objects a reconcile would apply.
    pub fn child_objects(
        &self,
        rendered: Vec<serde_json::Value>,
    ) -> crate::error::AppResult<Vec<DynamicObject>> {
        use crate::kubernetes::spec_editing::WithInjectedEnv;
        use crate::kubernetes::test_mode;
        let ns = self.namespace().unwrap_or_else(|| "default".to_string());
        let deploy_env_vars = self.deploy_env_vars();
        let mut objects = Vec::with_capacity(rendered.len());
        for resource in rendered {
            let mut obj: DynamicObject = serde_json::from_value(resource).map_err(|e| {
                crate::error::AppError::Internal(format!(
                    "JSON didn't look like a Kubernetes object (apiVersion/kind/metadata): {}",
                    e
                ))
            })?;

            if let Some(kind) = obj.types.as_ref().map(|t| t.kind.as_str()) {
                if test_mode::skips_kind(kind) {
                    log::debug!("Test mode: skipping {} {}", kind, obj.name_any());
                    continue;
                }
            }
            if test_mode::ENABLED && obj.metadata.namespace.is_some() {
                // Manifests name their production namespace; in test mode the
                // DeployConfig lives in the prefixed one and children follow it.
                obj.metadata.namespace = Some(ns.clone());
            }

            obj = obj.with_injected_env(&deploy_env_vars);

            self.ensure_owner_reference(&mut obj);
            self.ensure_labels(&mut obj);
            self.ensure_annotations(&mut obj);
            objects.push(obj);
        }
        Ok(objects)
    }

    #[allow(clippy::expect_used)]
    pub fn spec_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_string(&self.spec.spec).expect("Failed to serialize spec"));
        format!("{:x}", hasher.finalize())
    }

    pub fn owns(&self, obj: &DynamicObject) -> bool {
        let Some(owners) = &obj.metadata.owner_references else {
            return false;
        };

        #[allow(clippy::expect_used)]
        owners
            .iter()
            .any(|or| or.uid == self.uid().expect("DeployConfig should have a UID"))
    }

    pub fn child_is_up_to_date(&self, obj: &DynamicObject) -> bool {
        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, config } => obj
                .metadata
                .annotations
                .as_ref()
                .map(|a| {
                    a.get("artifactSha").is_some_and(|sha| sha == &artifact.sha)
                        && a.get("configSha").is_some_and(|sha| sha == &config.sha)
                })
                .unwrap_or(false),

            DeploymentState::DeployedOnlyConfig { config } => obj
                .metadata
                .annotations
                .as_ref()
                .map(|a| a.get("configSha").is_some_and(|sha| sha == &config.sha))
                .unwrap_or(false),

            DeploymentState::Undeployed => false,
        }
    }

    /// Ensure the annotations are set on a child resource
    pub fn ensure_annotations<T: ResourceExt>(&self, resource: &mut T) {
        let annotations = resource
            .meta_mut()
            .annotations
            .get_or_insert_with(BTreeMap::new);

        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, config } => {
                annotations.insert("artifactSha".to_string(), artifact.sha);
                annotations.insert("configSha".to_string(), config.sha);
            }
            DeploymentState::DeployedOnlyConfig { config } => {
                annotations.insert("configSha".to_string(), config.sha);
            }
            DeploymentState::Undeployed => {}
        }
    }

    /// Ensure the labels are set on a child resource
    pub fn ensure_labels<T: ResourceExt>(&self, resource: &mut T) {
        let labels = resource.meta_mut().labels.get_or_insert_with(BTreeMap::new);
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            "cicd-controller".to_string(),
        );
    }

    /// Ensure the owner reference is set on a child resource
    pub fn ensure_owner_reference<T: ResourceExt>(&self, resource: &mut T) {
        // Get the current owner references or create an empty vec
        let owner_refs = resource
            .meta_mut()
            .owner_references
            .get_or_insert_with(Vec::new);

        // Check if owner reference for this DeployConfig already exists
        let owner_ref_exists = owner_refs.iter().any(|ref_| {
            ref_.kind == DEPLOY_CONFIG_KIND
                && ref_.name == self.name_any()
                && ref_.api_version == "cicd.coolkev.com/v1"
        });

        // If it doesn't exist, add it
        if !owner_ref_exists {
            owner_refs.push(self.child_owner_reference());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, repo: &str) -> Repository {
        Repository {
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
    }

    fn config(config_repo: Repository, artifact_repo: Option<Repository>) -> DeployConfig {
        DeployConfig::new(
            "test",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "test".to_string(),
                    kind: "service".to_string(),
                    parameters: ParameterSource::sha_map(
                        artifact_repo.map(|r| r.with_branch("master")),
                    ),
                    selections: Selections::default(),
                    patches: vec![],
                    config: config_repo,
                    specs: vec![],
                },
            },
        )
    }

    #[test]
    fn references_config_repo() {
        let dc = config(repo("kj800x", "app"), None);
        assert!(dc.references_repo(&repo("kj800x", "app")));
        assert!(!dc.references_repo(&repo("kj800x", "other")));
        assert!(!dc.references_repo(&repo("someone-else", "app")));
    }

    #[test]
    fn references_artifact_repo() {
        let dc = config(
            repo("sqrt10pi", "wr-rs"),
            Some(repo("kj800x", "site-server")),
        );
        assert!(dc.references_repo(&repo("sqrt10pi", "wr-rs")));
        assert!(dc.references_repo(&repo("kj800x", "site-server")));
        assert!(!dc.references_repo(&repo("kj800x", "wr-rs")));
    }

    #[test]
    fn references_repo_is_case_insensitive() {
        let dc = config(repo("kj800x", "Hello-World"), None);
        assert!(dc.references_repo(&repo("KJ800X", "hello-world")));
    }

    #[test]
    fn references_repo_sees_parameter_sources() {
        let mut dc = config(repo("kj800x", "app"), None);
        dc.spec.spec.parameters.insert(
            SHA_PARAMETER.to_string(),
            ParameterSource::from(repo("kj800x", "image").with_branch("master")),
        );
        assert!(dc.references_repo(&repo("kj800x", "image")));
        assert!(!dc.references_repo(&repo("kj800x", "unrelated")));
    }

    fn dc_json(
        spec_extra: serde_json::Value,
        status_extra: serde_json::Value,
    ) -> serde_json::Value {
        let mut json = serde_json::json!({
            "apiVersion": "cicd.coolkev.com/v1",
            "kind": DEPLOY_CONFIG_KIND,
            "metadata": {"name": "cicd", "namespace": "cicd"},
            "spec": {
                "config": {"owner": "kj800x", "repo": "cicd"},
                "kind": "service",
                "team": "cluster-infra",
                "specs": []
            },
            "status": {
                "config": {"sha": "cfg", "branch": "master"},
                "orphaned": false
            }
        });
        if let (Some(spec), Some(extra)) = (json["spec"].as_object_mut(), spec_extra.as_object()) {
            spec.extend(extra.clone());
        }
        if let (Some(status), Some(extra)) =
            (json["status"].as_object_mut(), status_extra.as_object())
        {
            status.extend(extra.clone());
        }
        json
    }

    fn param_source() -> serde_json::Value {
        serde_json::json!({"parameters": {"SHA": {"type": "commit", "owner": "kj800x", "repo": "cicd", "branch": "master"}}})
    }

    fn param_value(sha: &str) -> serde_json::Value {
        serde_json::json!({"parameters": {"SHA": {"type": "commit", "value": sha, "branch": "master"}}})
    }

    fn deployed_sha(dc: &DeployConfig) -> Option<String> {
        match dc.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, .. } => Some(artifact.sha),
            _ => None,
        }
    }

    #[test]
    fn parameters_shape_reads() -> Result<(), serde_json::Error> {
        let dc: DeployConfig = serde_json::from_value(dc_json(param_source(), param_value("new")))?;
        assert_eq!(
            dc.artifact_repository().map(|r| r.branch),
            Some("master".into())
        );
        assert_eq!(deployed_sha(&dc), Some("new".into()));
        assert!(!dc.is_temporary_deployment());
        Ok(())
    }

    fn with_status(mut dc: DeployConfig, sha: &str, branch: Option<&str>) -> DeployConfig {
        let mut status = DeployConfigStatus {
            config: Some(ShaMaybeBranch {
                sha: "cfg".into(),
                branch: Some("master".into()),
            }),
            ..Default::default()
        };
        status.parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::from(ShaMaybeBranch {
                sha: sha.into(),
                branch: branch.map(String::from),
            }),
        );
        dc.status = Some(status);
        dc
    }

    #[test]
    fn selection_is_derived_from_the_deployed_branch() {
        use crate::kubernetes::selections::Mode;
        let base = config(repo("kj800x", "app"), Some(repo("kj800x", "app")));

        let tracking = with_status(base.clone(), "abc", Some("master"));
        assert_eq!(tracking.selection(SHA_PARAMETER).mode(), Mode::Default);
        assert!(!tracking.is_temporary_deployment());

        let branch = with_status(base.clone(), "abc", Some("feature"));
        assert_eq!(
            branch.selection(SHA_PARAMETER).mode(),
            Mode::Track("feature")
        );
        assert!(
            branch.is_temporary_deployment(),
            "derived branch overrides are temporary"
        );

        let pinned = with_status(base.clone(), "abc", None);
        assert_eq!(pinned.selection(SHA_PARAMETER).mode(), Mode::Pin("abc"));
        assert!(
            !pinned.is_temporary_deployment(),
            "derived pins are standing"
        );

        assert!(
            !base.is_temporary_deployment(),
            "undeployed is never temporary"
        );
    }

    #[test]
    fn templates_read_both_stored_shapes() {
        let bare = serde_json::json!({"kind": "Deployment", "metadata": {"name": "web"}});
        let t = Template::from_stored(&bare);
        assert_eq!(t.file, None);
        assert_eq!(t.manifest, bare);

        let stored = Template::stored("deployment.yaml", bare.clone());
        let t = Template::from_stored(&stored);
        assert_eq!(t.file.as_deref(), Some("deployment.yaml"));
        assert_eq!(t.manifest, bare);

        let mut dc = config(repo("kj800x", "app"), None);
        dc.spec.spec.specs = vec![stored, bare.clone()];
        assert_eq!(dc.resource_specs(), vec![bare.clone(), bare]);
        assert!(dc.supports_bounce());
    }

    #[test]
    fn value_parameters_resolve_to_default_or_pin() {
        let mut dc = config(repo("kj800x", "app"), Some(repo("kj800x", "app")));
        dc.spec.spec.parameters.insert(
            "REPLICAS".into(),
            ParameterSource::Value {
                default: "2".into(),
            },
        );
        assert_eq!(
            dc.resolve_value_parameters()
                .get("REPLICAS")
                .map(|v| v.rendered()),
            Some("2".into())
        );
        dc.spec
            .spec
            .selections
            .insert("REPLICAS".into(), Selection::pin("5", Durability::Standing));
        assert_eq!(
            dc.resolve_value_parameters()
                .get("REPLICAS")
                .map(|v| v.rendered()),
            Some("5".into())
        );
        assert!(
            !dc.resolve_value_parameters().contains_key(SHA_PARAMETER),
            "feeds are not static"
        );

        // Deployed value parameters show up in env vars with their mode.
        let mut status = DeployConfigStatus::default();
        status.parameters.insert(
            "REPLICAS".into(),
            ParameterValue::Value { value: "5".into() },
        );
        dc.status = Some(status);
        let env: std::collections::BTreeMap<String, String> =
            dc.deploy_env_vars().into_iter().collect();
        assert_eq!(env["CICD_PARAM_REPLICAS"], "5");
        assert_eq!(env["CICD_PARAM_REPLICAS_MODE"], "pin");
        assert!(!env.contains_key("CICD_PARAM_REPLICAS_CHANNEL"));
    }

    #[test]
    fn render_substitutes_values_and_refuses_missing_declared_ones() {
        let mut dc = with_status(
            config(repo("kj800x", "app"), Some(repo("kj800x", "app"))),
            "abc",
            Some("master"),
        );
        dc.spec.spec.specs = vec![serde_json::json!({
            "kind": "Deployment",
            "spec": { "image": "app:commit-$SHA", "cmd": "echo $HOME" }
        })];
        let rendered = dc.render_manifests().unwrap_or_default();
        assert_eq!(rendered[0]["spec"]["image"], "app:commit-abc");
        assert_eq!(rendered[0]["spec"]["cmd"], "echo $HOME");

        // Declared, referenced, but not deployed: refuse.
        let mut undeployed = config(repo("kj800x", "app"), Some(repo("kj800x", "app")));
        undeployed.spec.spec.specs = dc.spec.spec.specs.clone();
        assert!(undeployed.render_manifests().is_err());

        // Referenced but not declared: not ours, rendered as is.
        let mut plain = config(repo("kj800x", "app"), None);
        plain.spec.spec.specs = vec![serde_json::json!({"cmd": "$SHA"})];
        assert_eq!(
            plain.render_manifests().unwrap_or_default()[0]["cmd"],
            "$SHA"
        );
    }

    #[test]
    fn env_vars_describe_each_parameter_and_temporariness() {
        let base = config(repo("kj800x", "app"), Some(repo("kj800x", "app")));
        let env = |dc: &DeployConfig| -> std::collections::BTreeMap<String, String> {
            dc.deploy_env_vars().into_iter().collect()
        };

        let tracking = env(&with_status(base.clone(), "abc", Some("master")));
        assert_eq!(tracking["CICD_PARAM_SHA"], "abc");
        assert_eq!(tracking["CICD_PARAM_SHA_MODE"], "track");
        assert_eq!(tracking["CICD_PARAM_SHA_CHANNEL"], "master");
        assert_eq!(tracking["CICD_TEMPORARY_DEPLOY"], "false");
        assert_eq!(tracking["CICD_CONFIG_SHA"], "cfg");
        assert!(
            !tracking.contains_key("CICD_ARTIFACT_SHA"),
            "old names are gone"
        );
        assert!(!tracking.contains_key("CICD_NON_LATEST_DEPLOY"));

        let branch = env(&with_status(base.clone(), "abc", Some("feature")));
        assert_eq!(branch["CICD_PARAM_SHA_MODE"], "override");
        assert_eq!(branch["CICD_PARAM_SHA_CHANNEL"], "feature");
        assert_eq!(branch["CICD_TEMPORARY_DEPLOY"], "true");

        let pinned = env(&with_status(base.clone(), "abc", None));
        assert_eq!(pinned["CICD_PARAM_SHA_MODE"], "pin");
        assert!(!pinned.contains_key("CICD_PARAM_SHA_CHANNEL"));
        assert_eq!(pinned["CICD_TEMPORARY_DEPLOY"], "false");

        let undeployed = env(&base);
        assert_eq!(undeployed["CICD_TEMPORARY_DEPLOY"], "false");
        assert!(!undeployed.contains_key("CICD_PARAM_SHA"));
    }

    #[test]
    fn explicit_selection_wins_over_derivation() {
        use crate::kubernetes::selections::Mode;
        let mut dc = with_status(
            config(repo("kj800x", "app"), Some(repo("kj800x", "app"))),
            "abc",
            None,
        );
        dc.spec.spec.selections.insert(
            SHA_PARAMETER.into(),
            Selection::pin("abc", Durability::Temporary),
        );
        assert_eq!(dc.selection(SHA_PARAMETER).mode(), Mode::Pin("abc"));
        assert!(
            dc.is_temporary_deployment(),
            "explicit durability is respected"
        );
        let out = serde_json::to_value(&dc).unwrap_or_default();
        assert_eq!(out["spec"]["selections"]["SHA"]["pin"]["value"], "abc");
        assert_eq!(out["spec"]["selections"]["SHA"]["durability"], "temporary");
    }

    #[test]
    fn no_sha_anywhere_is_config_only() -> Result<(), serde_json::Error> {
        let dc: DeployConfig =
            serde_json::from_value(dc_json(serde_json::json!({}), serde_json::json!({})))?;
        assert!(dc.artifact_repository().is_none());
        assert!(matches!(
            dc.deployment_state(),
            DeploymentState::DeployedOnlyConfig { .. }
        ));
        Ok(())
    }

    #[test]
    fn empty_maps_are_not_serialized() -> Result<(), serde_json::Error> {
        let dc: DeployConfig =
            serde_json::from_value(dc_json(serde_json::json!({}), serde_json::json!({})))?;
        let out = serde_json::to_value(&dc)?;
        assert!(out["spec"].get("parameters").is_none());
        assert!(out["status"].get("parameters").is_none());
        Ok(())
    }
}
