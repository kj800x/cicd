//! How temporary deployments show on the deploy page: the alert in a
//! temporary config's preview and the strip listing every temporary
//! deployment under the nav.

use maud::{html, Markup};

use crate::kubernetes::parameters::SHA_PARAMETER;
use crate::kubernetes::selections::Mode;
use crate::kubernetes::DeployConfig;
use crate::web::formatting;
use crate::web::preview::render_durability_badge;
use kube::ResourceExt;

/// One clause per temporary override: `SHA tracking fix/x`, `NGINX pinned
/// to 1.27.4`, each with its badge and note.
fn render_temporary_overrides(config: &DeployConfig) -> Vec<Markup> {
    let mut names: Vec<String> = config.spec.spec.selections.keys().cloned().collect();
    if !names.iter().any(|n| n == SHA_PARAMETER) {
        names.push(SHA_PARAMETER.to_string());
    }
    names.sort_by_key(|n| (n != SHA_PARAMETER, n.clone()));
    names
        .iter()
        .filter_map(|name| {
            let selection = config.selection(name);
            if !selection.is_temporary() {
                return None;
            }
            let what = match selection.mode() {
                Mode::Track(channel) => html! { "tracking " span.mono { (channel) } },
                Mode::Pin(value) => {
                    html! { "pinned to " span.mono title=(value) { (formatting::format_short_sha(value)) } }
                }
                Mode::Default => return None,
            };
            Some(html! {
                span.temporary-item {
                    span.mono { (name) } " " (what) " "
                    (render_durability_badge(selection.durability))
                    @if let Some(note) = &selection.note { span.muted { " · \u{201c}" (note) "\u{201d}" } }
                }
            })
        })
        .collect()
}

/// The warning shown in the preview while a config is a temporary deployment.
pub fn render_temporary_alert(config: &DeployConfig) -> Markup {
    let name = config.name_any();
    let overrides = render_temporary_overrides(config);
    let patches = config
        .spec
        .spec
        .patches
        .iter()
        .filter(|p| p.is_temporary())
        .count();
    let since = config.temporary_since();
    html! {
        div.alert.alert-warning {
            div class="alert-header" {
                i class="fa fa-flask" {}
                " Temporary deployment"
            }
            div class="alert-content" {
                div class="details" {
                    (name) " has "
                    @for (i, item) in overrides.iter().enumerate() {
                        @if i > 0 { ", " }
                        (item)
                    }
                    @if patches > 0 {
                        @if !overrides.is_empty() { " and " }
                        (patches) " temporary patch" @if patches != 1 { "es" }
                    }
                    @if let Some(since) = since {
                        span.muted { " (since " (formatting::format_ago_short(since)) ")" }
                    }
                    ". Autodeploy is suppressed while a temporary deployment is active."
                }
            }
        }
    }
}

/// The full-bleed strip under the nav listing temporary deployments, each
/// linked to the action that ends it.
pub fn render_temporary_strip(configs: &[DeployConfig]) -> Markup {
    let temporary: Vec<&DeployConfig> = configs
        .iter()
        .filter(|c| c.is_temporary_deployment())
        .collect();
    if temporary.is_empty() {
        return html! {};
    }
    html! {
        div.page-strip.page-strip--warn {
            i class="fa fa-flask" {}
            span {
                "Temporary deployments: "
                @for (i, config) in temporary.iter().enumerate() {
                    @if i > 0 { ", " }
                    a href=(format!("/deploy?selected={}&action=end-temporary", config.name_any())) { (config.name_any()) }
                    @if let Some(since) = config.temporary_since() {
                        span.muted { " · since " (formatting::format_ago_short(since)) }
                    }
                }
            }
        }
    }
}
