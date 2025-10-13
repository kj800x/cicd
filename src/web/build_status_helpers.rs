use crate::db::BuildStatus;

/// Get the CSS class for a build status indicator
pub fn build_status_class(status: &BuildStatus) -> &'static str {
    match status {
        BuildStatus::Success => "status-success",
        BuildStatus::Failure => "status-failure",
        BuildStatus::Pending => "status-pending",
        BuildStatus::None => "status-none",
    }
}

/// Get the CSS class for a build status background
pub fn build_status_bg_class(status: &BuildStatus) -> &'static str {
    match status {
        BuildStatus::Success => "bg-success",
        BuildStatus::Failure => "bg-failure",
        BuildStatus::Pending => "bg-pending",
        BuildStatus::None => "bg-none",
    }
}

/// Get the CSS class for a build card status
pub fn build_card_status_class(status: &BuildStatus) -> &'static str {
    match status {
        BuildStatus::Success => "card-status-success",
        BuildStatus::Failure => "card-status-failure",
        BuildStatus::Pending => "card-status-pending",
        BuildStatus::None => "card-status-none",
    }
}
