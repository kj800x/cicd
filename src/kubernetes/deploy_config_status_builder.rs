use crate::kubernetes::repo::ShaMaybeBranch;

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
            if let Some(artifact) = artifact {
                status["artifact"] = serde_json::json!({});
                status["artifact"]["sha"] = artifact.sha.into();
                status["artifact"]["branch"] = artifact.branch.into();
            } else {
                status["artifact"] = serde_json::Value::Null;
            }
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
