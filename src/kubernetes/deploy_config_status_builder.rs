use crate::prelude::DeployConfigStatus;

/// Builder for patch updates to DeployConfigStatus.
/// Since the values are optional, we need to use Option<Option<String>> to represent them in this builder.
/// The outer Option is for if this builder will be setting the value in the patch, the inner Option is the actual value that will be set on the status.
/// For example, owner: None means no patch for owner, owner: Some(None) means patch for owner but set to None, owner: Some(Some(owner)) means patch for owner and set to the given value.
#[derive(Clone, Debug, Default)]
pub struct DeployConfigStatusBuilder {
    autodeploy: Option<Option<bool>>,
    config_owner: Option<Option<String>>,
    config_repo: Option<Option<String>>,
    config_sha: Option<Option<String>>,
    artifact_branch: Option<Option<String>>,
    artifact_current_sha: Option<Option<String>>,
    artifact_latest_sha: Option<Option<String>>,
    artifact_wanted_sha: Option<Option<String>>,
}

// FIXME: We shouldn't need to do this, because you don't need the existing status to build a patch for a deploy event.
impl From<&DeployConfigStatus> for DeployConfigStatusBuilder {
    fn from(val: &DeployConfigStatus) -> Self {
        Self {
            autodeploy: Some(val.autodeploy),
            config_owner: Some(val.config.as_ref().and_then(|c| c.owner.clone())),
            config_repo: Some(val.config.as_ref().and_then(|c| c.repo.clone())),
            config_sha: Some(val.config.as_ref().and_then(|c| c.sha.clone())),
            artifact_branch: Some(val.artifact.as_ref().and_then(|a| a.branch.clone())),
            artifact_current_sha: Some(val.artifact.as_ref().and_then(|a| a.current_sha.clone())),
            artifact_latest_sha: Some(val.artifact.as_ref().and_then(|a| a.latest_sha.clone())),
            artifact_wanted_sha: Some(val.artifact.as_ref().and_then(|a| a.wanted_sha.clone())),
        }
    }
}

impl From<DeployConfigStatusBuilder> for serde_json::Value {
    fn from(val: DeployConfigStatusBuilder) -> Self {
        let mut config = serde_json::json!({});
        if let Some(owner) = val.config_owner {
            config["owner"] = owner.into();
        }
        if let Some(repo) = val.config_repo {
            config["repo"] = repo.into();
        }
        if let Some(sha) = val.config_sha {
            config["sha"] = sha.into();
        }

        let mut artifact = serde_json::json!({});
        if let Some(branch) = val.artifact_branch {
            artifact["branch"] = branch.into();
        }
        if let Some(current_sha) = val.artifact_current_sha {
            artifact["currentSha"] = current_sha.into();
        }
        if let Some(latest_sha) = val.artifact_latest_sha {
            artifact["latestSha"] = latest_sha.into();
        }
        if let Some(wanted_sha) = val.artifact_wanted_sha {
            artifact["wantedSha"] = wanted_sha.into();
        }

        let mut status = serde_json::json!({});
        if let Some(autodeploy) = val.autodeploy {
            status["autodeploy"] = autodeploy.into();
        }
        if let Some(config) = config.as_object() {
            if !config.is_empty() {
                status["config"] = serde_json::Value::Object(config.clone());
            }
        }
        if let Some(artifact) = artifact.as_object() {
            if !artifact.is_empty() {
                status["artifact"] = serde_json::Value::Object(artifact.clone());
            }
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

    pub fn with_autodeploy(mut self, autodeploy: bool) -> Self {
        self.autodeploy = Some(Some(autodeploy));
        self
    }

    pub fn with_config_owner(mut self, owner: String) -> Self {
        self.config_owner = Some(Some(owner));
        self
    }

    pub fn with_config_repo(mut self, repo: String) -> Self {
        self.config_repo = Some(Some(repo));
        self
    }

    pub fn with_config_sha(mut self, sha: String) -> Self {
        self.config_sha = Some(Some(sha));
        self
    }

    pub fn with_artifact_branch(mut self, branch: String) -> Self {
        self.artifact_branch = Some(Some(branch));
        self
    }

    pub fn with_artifact_current_sha(mut self, current_sha: String) -> Self {
        self.artifact_current_sha = Some(Some(current_sha));
        self
    }

    pub fn with_null_artifact_current_sha(mut self) -> Self {
        self.artifact_current_sha = Some(None);
        self
    }

    pub fn with_artifact_latest_sha(mut self, latest_sha: String) -> Self {
        self.artifact_latest_sha = Some(Some(latest_sha));
        self
    }

    pub fn with_artifact_wanted_sha(mut self, wanted_sha: String) -> Self {
        self.artifact_wanted_sha = Some(Some(wanted_sha));
        self
    }

    pub fn get_artifact_wanted_sha(&self) -> Option<&str> {
        self.artifact_wanted_sha
            .as_ref()
            .and_then(|s| s.as_ref().map(|s| s.as_str()))
    }
}
