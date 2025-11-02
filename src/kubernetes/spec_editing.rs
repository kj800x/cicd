use kube::api::DynamicObject;

pub trait WithInterpolatedVersion {
    fn with_interpolated_version(&self, version: &str) -> Self;
}

pub trait WithVersion {
    fn with_version(&self, version: &str) -> Self;
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
    /// Interpolates the data with the given version
    fn with_version(&self, version: &str) -> Self {
        let mut obj = self.clone();

        obj.data = obj.data.with_interpolated_version(version);
        obj
    }
}
