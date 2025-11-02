use crate::kubernetes::{deploy_config::DEPLOY_CONFIG_KIND, DeployConfig};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{api::DynamicObject, Resource, ResourceExt};
use std::collections::BTreeMap;

pub trait WithInterpolatedVersion {
    fn with_interpolated_version(&self, version: &str) -> Self;
}

pub trait WithVersion {
    fn with_version(&self, version: &str) -> Self;
    fn get_sha(&self) -> Option<&str>;
}

impl WithInterpolatedVersion for serde_json::Value {
    fn with_interpolated_version(&self, version: &str) -> Self {
        match self {
            serde_json::Value::Object(json) => {
                let mut new_json = serde_json::Map::new();
                for (key, value) in json {
                    new_json.insert(key.clone(), value.with_interpolated_version(version));
                }
                serde_json::Value::Object(new_json)
            }
            serde_json::Value::Array(array) => {
                let mut new_array = Vec::new();
                for value in array {
                    new_array.push(value.with_interpolated_version(version));
                }
                serde_json::Value::Array(new_array)
            }
            serde_json::Value::String(string) => {
                serde_json::Value::String(string.replace("$SHA", version))
            }
            _ => self.clone(),
        }
    }
}

impl WithInterpolatedVersion for serde_json::Map<String, serde_json::Value> {
    fn with_interpolated_version(&self, version: &str) -> Self {
        #[allow(clippy::expect_used)]
        serde_json::Value::Object(self.clone())
            .with_interpolated_version(version)
            .as_object()
            .expect("with_interpolated_version should return an object")
            .clone()
    }
}

impl WithVersion for DynamicObject {
    /// Sets metadata.annotations.currentSha to the given version and interpolates the data with the given version
    fn with_version(&self, version: &str) -> Self {
        let mut obj = self.clone();

        if obj.meta_mut().annotations.is_none() {
            obj.meta_mut().annotations = Some(BTreeMap::new());
        }

        // FIXME: Should we still be trying to set a currentSha annotation after the latest architecture changes?
        #[allow(clippy::expect_used)]
        obj.meta_mut()
            .annotations
            .as_mut()
            .expect("Annotations should exist after initialization")
            .insert("currentSha".to_owned(), version.to_owned());

        obj.data = obj.data.with_interpolated_version(version);
        obj
    }

    fn get_sha(&self) -> Option<&str> {
        if let Some(annotations) = &self.meta().annotations {
            annotations.get("currentSha").map(|s| s.as_str())
        } else {
            None
        }
    }
}
