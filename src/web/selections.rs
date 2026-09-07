//! How a config's selection shows on the deploy page: the status line, the
//! temporary-deployment alert, and the strip listing temporary deployments.

use maud::{html, Markup};

use crate::kubernetes::parameters::SHA_PARAMETER;
use crate::kubernetes::selections::{Mode, Selection};
use crate::kubernetes::DeployConfig;
use kube::ResourceExt;

/// One line describing what the `SHA` parameter is following.
pub fn render_selection_summary(config: &DeployConfig) -> Markup {
    let selection: Selection = config.selection(SHA_PARAMETER);
    let default_branch = config
        .artifact_repository()
        .map(|r| r.branch)
        .unwrap_or_else(|| "master".to_string());
    html! {
        @match selection.mode() {
            Mode::Default => { "tracking " strong { (default_branch) } " (default)" }
            Mode::Track(branch) => {
                "tracking " strong { (branch) } " " (render_durability(&selection))
            }
            Mode::Pin(value) => {
                "pinned to " strong { (crate::web::formatting::format_short_sha(value)) } " " (render_durability(&selection))
            }
        }
        @if let Some(note) = &selection.note {
            " · " span.muted { (note) }
        }
        @if let Some(by) = &selection.by {
            " " span.muted { "(" (by) ")" }
        }
    }
}

fn render_durability(selection: &Selection) -> Markup {
    html! {
        span class=(format!("durability durability-{}", selection.durability.as_str())) {
            (selection.durability.as_str())
        }
    }
}

/// The warning shown in the preview while a config is a temporary deployment.
/// Says what is temporary and links to the one action that ends all of it.
pub fn render_temporary_alert(config: &DeployConfig) -> Markup {
    let changes = crate::deploys::temporary_changes(config);
    let name = config.name_any();
    html! {
        div.alert.alert-warning {
            div class="alert-header" {
                i class="fa fa-flask" {}
                " Temporary deployment"
            }
            div class="alert-content" {
                div class="details" {
                    "This config is " (render_selection_summary(config)) ". "
                    "Autodeploy stays off until every temporary change is gone. "
                    a href=(format!("/deploy?selected={name}&action=end-temporary")) {
                        "End temporary deployment"
                    }
                    " will " (changes.summary()) " and deploy latest."
                }
            }
        }
    }
}

/// One line at the top of the deploy page listing temporary deployments.
pub fn render_temporary_strip(configs: &[DeployConfig]) -> Markup {
    let temporary: Vec<&DeployConfig> = configs
        .iter()
        .filter(|c| c.is_temporary_deployment())
        .collect();
    if temporary.is_empty() {
        return html! {};
    }
    html! {
        div.temporary-strip {
            i class="fa fa-flask" {}
            " Temporary deployments: "
            @for (i, config) in temporary.iter().enumerate() {
                @if i > 0 { ", " }
                a href=(format!("/deploy?selected={}", config.name_any())) { (config.name_any()) }
            }
        }
    }
}
