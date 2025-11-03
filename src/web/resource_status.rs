trait ResourceStatus {
    fn format(&self) -> Markup;
}

impl ResourceStatus for serde_json::Value {
    fn format(&self) -> Markup {
        html! {
            div {
                b {
                    (spec.get("kind").map(|k| k.as_str().unwrap_or_default()).unwrap_or_default())
                }
                ": "
                (spec.get("metadata").and_then(|m| m.get("name").map(|n| n.as_str().unwrap_or_default())).unwrap_or_default())
            }
        }
    }
}

trait HasResourceStatuses {
    fn resource_status(&self) -> Markup;
}

impl HasResourceStatuses for DeployConfig {
    /// Formats the current resource status
    fn resource_status(&self) -> Markup {
        html! {
            ul {
                @for spec in self.current_config.resource_specs() {
                    li {
                        (self.format_resource(&spec))
                    }
                }
            }
        }
    }
}
