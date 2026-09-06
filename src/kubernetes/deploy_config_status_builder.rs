use crate::kubernetes::{parameters::SHA_PARAMETER, repo::ShaMaybeBranch};

/// Builder for patch updates to DeployConfigStatus.
/// Since the values are optional, we need to use Option<Option<String>> to represent them in this builder.
/// The outer Option is for if this builder will be setting the value in the patch, the inner Option is the actual value that will be set on the status.
/// For example, owner: None means no patch for owner, owner: Some(None) means patch for owner but set to None, owner: Some(Some(owner)) means patch for owner and set to the given value.
#[derive(Clone, Debug, Default)]
pub struct DeployConfigStatusBuilder {
    autodeploy: Option<Option<bool>>,
    orphaned: Option<Option<bool>>,
    artifact: Option<Option<ShaMaybeBranch>>,
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

        if let Some(artifact) = val.artifact {
            // Built by hand so that an absent branch becomes an explicit
            // null; a nested merge patch would otherwise keep the previous
            // deploy's branch. A `null` entry deletes the key.
            status["parameters"] = match artifact {
                Some(artifact) => serde_json::json!({
                    SHA_PARAMETER: {
                        "type": "commit",
                        "value": artifact.sha,
                        "branch": artifact.branch,
                    }
                }),
                None => serde_json::json!({ SHA_PARAMETER: null }),
            };
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

    pub fn with_artifact(mut self, artifact: Option<ShaMaybeBranch>) -> Self {
        self.artifact = Some(artifact);
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
    fn untouched_fields_are_not_in_the_patch() {
        let patch: Value = DeployConfigStatusBuilder::new()
            .with_orphaned(Some(true))
            .into();
        assert_eq!(patch, json!({"status": {"orphaned": true}}));
    }
}
