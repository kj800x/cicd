use std::collections::BTreeMap;

use crate::kubernetes::{
    parameters::{ParameterValue, ParameterValues, SHA_PARAMETER},
    repo::ShaMaybeBranch,
};

/// Builder for patch updates to DeployConfigStatus.
/// Since the values are optional, we need to use Option<Option<String>> to represent them in this builder.
/// The outer Option is for if this builder will be setting the value in the patch, the inner Option is the actual value that will be set on the status.
/// For example, owner: None means no patch for owner, owner: Some(None) means patch for owner but set to None, owner: Some(Some(owner)) means patch for owner and set to the given value.
#[derive(Clone, Debug, Default)]
pub struct DeployConfigStatusBuilder {
    autodeploy: Option<Option<bool>>,
    orphaned: Option<Option<bool>>,
    /// Per-parameter patches: `None` deletes the key.
    parameters: BTreeMap<String, Option<ParameterValue>>,
    /// Replace the whole parameters map with null (undeploy).
    clear_parameters: bool,
    config: Option<Option<ShaMaybeBranch>>,
}

impl From<DeployConfigStatusBuilder> for serde_json::Value {
    fn from(val: DeployConfigStatusBuilder) -> Self {
        let mut status = serde_json::json!({});

        if let Some(config) = val.config {
            if let Some(config) = config {
                status["config"] = serde_json::json!({});
                status["config"]["sha"] = config.sha.into();
                status["config"]["branch"] = config.branch.into();
            } else {
                status["config"] = serde_json::Value::Null;
            }
        }

        if val.clear_parameters {
            status["parameters"] = serde_json::Value::Null;
        } else if !val.parameters.is_empty() {
            // Built by hand so that an absent branch becomes an explicit
            // null; a nested merge patch would otherwise keep the previous
            // deploy's branch. A `null` entry deletes the key.
            let mut params = serde_json::json!({});
            for (name, value) in &val.parameters {
                params[name] = match value {
                    Some(ParameterValue::Commit { value, branch }) => serde_json::json!({
                        "type": "commit",
                        "value": value,
                        "branch": branch,
                    }),
                    Some(ParameterValue::Value { value }) => serde_json::json!({
                        "type": "value",
                        "value": value,
                    }),
                    None => serde_json::Value::Null,
                };
            }
            status["parameters"] = params;
        }

        if let Some(autodeploy) = val.autodeploy {
            status["autodeploy"] = autodeploy.into();
        }

        if let Some(orphaned) = val.orphaned {
            status["orphaned"] = orphaned.into();
        }

        serde_json::json!({
            "status": status,
        })
    }
}

impl DeployConfigStatusBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_autodeploy(mut self, autodeploy: Option<bool>) -> Self {
        self.autodeploy = Some(autodeploy);
        self
    }

    pub fn with_config(mut self, config: Option<ShaMaybeBranch>) -> Self {
        self.config = Some(config);
        self
    }

    /// Patch the legacy artifact, i.e. the `SHA` parameter.
    pub fn with_artifact(self, artifact: Option<ShaMaybeBranch>) -> Self {
        self.with_parameter(SHA_PARAMETER, artifact.map(ParameterValue::from))
    }

    /// Set or delete (`None`) one parameter's deployed value.
    pub fn with_parameter(mut self, name: &str, value: Option<ParameterValue>) -> Self {
        self.parameters.insert(name.to_string(), value);
        self
    }

    /// Set every parameter in `values`.
    pub fn with_parameters(mut self, values: ParameterValues) -> Self {
        for (name, value) in values {
            self.parameters.insert(name, Some(value));
        }
        self
    }

    /// Delete every deployed parameter value (undeploy).
    pub fn clear_parameters(mut self) -> Self {
        self.clear_parameters = true;
        self
    }

    pub fn with_orphaned(mut self, orphaned: Option<bool>) -> Self {
        self.orphaned = Some(orphaned);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn artifact_is_written_as_the_sha_parameter() {
        let patch: Value = DeployConfigStatusBuilder::new()
            .with_artifact(Some(ShaMaybeBranch {
                sha: "abc".into(),
                branch: Some("master".into()),
            }))
            .into();
        assert_eq!(
            patch,
            json!({"status": {
                "parameters": {"SHA": {"type": "commit", "value": "abc", "branch": "master"}},
            }})
        );
    }

    #[test]
    fn absent_branch_is_an_explicit_null() {
        let patch: Value = DeployConfigStatusBuilder::new()
            .with_artifact(Some(ShaMaybeBranch {
                sha: "abc".into(),
                branch: None,
            }))
            .into();
        assert_eq!(patch["status"]["parameters"]["SHA"]["branch"], Value::Null);
    }

    #[test]
    fn undeploy_deletes_the_sha_parameter() {
        let patch: Value = DeployConfigStatusBuilder::new().with_artifact(None).into();
        assert_eq!(patch, json!({"status": {"parameters": {"SHA": null}}}));
    }

    #[test]
    fn value_parameters_and_clearing() {
        let patch: Value = DeployConfigStatusBuilder::new()
            .with_parameters(ParameterValues::from([(
                "REPLICAS".to_string(),
                ParameterValue::Value { value: "3".into() },
            )]))
            .with_artifact(Some(ShaMaybeBranch {
                sha: "abc".into(),
                branch: None,
            }))
            .into();
        assert_eq!(
            patch["status"]["parameters"]["REPLICAS"],
            json!({"type": "value", "value": "3"})
        );
        assert_eq!(patch["status"]["parameters"]["SHA"]["value"], "abc");

        let cleared: Value = DeployConfigStatusBuilder::new().clear_parameters().into();
        assert_eq!(cleared, json!({"status": {"parameters": null}}));
    }

    #[test]
    fn untouched_fields_are_not_in_the_patch() {
        let patch: Value = DeployConfigStatusBuilder::new()
            .with_orphaned(Some(true))
            .into();
        assert_eq!(patch, json!({"status": {"orphaned": true}}));
    }
}
