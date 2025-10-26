use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum BuildStatus {
    None,
    Pending,
    Success,
    Failure,
}

impl From<Option<String>> for BuildStatus {
    fn from(s: Option<String>) -> Self {
        match s {
            Some(s) => match s.as_str() {
                "None" => BuildStatus::None,
                "Pending" => BuildStatus::Pending,
                "Success" => BuildStatus::Success,
                "Failure" => BuildStatus::Failure,
                _ => BuildStatus::None,
            },
            None => BuildStatus::None,
        }
    }
}

impl BuildStatus {
    pub fn of(status: &str, conclusion: &Option<&str>) -> Self {
        match status {
            "queued" => BuildStatus::Pending,
            "completed" => match conclusion {
                Some("success") => BuildStatus::Success,
                Some("failure") => BuildStatus::Failure,
                _ => BuildStatus::None,
            },
            _ => BuildStatus::None,
        }
    }
}

impl Into<String> for BuildStatus {
    fn into(self) -> String {
        match self {
            BuildStatus::None => "None".to_string(),
            BuildStatus::Pending => "Pending".to_string(),
            BuildStatus::Success => "Success".to_string(),
            BuildStatus::Failure => "Failure".to_string(),
        }
    }
}
