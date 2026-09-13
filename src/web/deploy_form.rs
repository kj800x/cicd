//! The left column of the deploy page: the config picker, the action
//! chooser, and, in advanced mode, one selector per parameter, the patch
//! list and the durability toggle. One GET form carries every choice (the
//! inputs point at it with `form=`, so the patch forms can sit between
//! them without nesting); one POST form mirrors the choices and holds the
//! button.

use std::collections::HashMap;

use kube::ResourceExt;
use maud::{html, Markup};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::git_branch::GitBranch;
use crate::db::git_commit::GitCommit;
use crate::db::git_repo::GitRepo;
use crate::kubernetes::parameters::{ParameterSource, SHA_PARAMETER};
use crate::kubernetes::selections::{Durability, Mode};
use crate::kubernetes::DeployConfig;
use crate::web::formatting;
use crate::web::preview::{render_durability_badge, Prepared, TagResolutions};
use crate::web::Action;

const ACTION_FORM: &str = "actionForm";

/// What the form shows for one parameter: the radio that is checked and
/// the text in the box under it.
struct Shown {
    kind: &'static str,
    typed: String,
}

/// The current effective selection, or the choice the form already sent.
fn shown_choice(config: &DeployConfig, action: &Action, name: &str) -> Shown {
    if let Some(choice) = action.choices().and_then(|c| c.get(name)) {
        return Shown {
            kind: choice.kind(),
            typed: choice.typed().unwrap_or_default().to_string(),
        };
    }
    let selection = config.selection(name);
    match selection.mode() {
        Mode::Default => Shown {
            kind: "default",
            typed: String::new(),
        },
        Mode::Track(channel) => Shown {
            kind: "track",
            typed: channel.to_string(),
        },
        Mode::Pin(value) => Shown {
            kind: "pin",
            typed: value.to_string(),
        },
    }
}

/// `resolves to 9c31be7 · 12m ago · "retry uploads on 504"`, for a branch
/// or a pinned commit of the artifact repo.
fn describe_commit_choice(
    conn: &PooledConnection<SqliteConnectionManager>,
    config: &DeployConfig,
    shown: &Shown,
    default_branch: &str,
) -> Markup {
    let Some(repository) = config.artifact_repository() else {
        return html! {};
    };
    let Some(repo) = GitRepo::get_by_name(&repository.owner, &repository.repo, conn)
        .ok()
        .flatten()
    else {
        return html! { span.muted { (repository.owner) "/" (repository.repo) " is not a known repository" } };
    };
    let commit_line = |commit: &GitCommit| {
        let first_line = commit
            .message
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        html! {
            "resolves to "
            strong.mono title=(commit.sha) { (formatting::format_short_sha(&commit.sha)) }
            span.faint { " · " (formatting::format_ago_short(commit.timestamp)) " · \u{201c}" (first_line) "\u{201d}" }
        }
    };
    match shown.kind {
        "pin" => {
            let sha = shown.typed.trim();
            if sha.is_empty() {
                return html! { span.muted { "type a commit sha" } };
            }
            let found = if sha.len() == 40 {
                GitCommit::get_by_sha(sha, repo.id, conn).ok().flatten()
            } else {
                match GitCommit::find_by_prefix(&sha.to_ascii_lowercase(), repo.id, conn) {
                    Ok(matches) if matches.len() == 1 => matches.into_iter().next(),
                    Ok(matches) if matches.len() > 1 => {
                        return html! { span.muted { (sha) " is ambiguous; type more of it" } }
                    }
                    _ => None,
                }
            };
            match found {
                Some(commit) => commit_line(&commit),
                None => html! { span.muted { "no known commit starts with " (sha) } },
            }
        }
        _ => {
            let branch_name = if shown.kind == "track" && !shown.typed.trim().is_empty() {
                shown.typed.trim().to_string()
            } else if shown.kind == "track" {
                return html! { span.muted { "type a branch" } };
            } else {
                default_branch.to_string()
            };
            let Some(branch) = GitBranch::get_by_name(&branch_name, repo.id, conn)
                .ok()
                .flatten()
            else {
                return html! { span.muted { "no branch named " (branch_name) } };
            };
            match branch.latest_successful_build(conn).ok().flatten() {
                Some(commit) => commit_line(&commit),
                None => html! { span.muted { "no successful build on " (branch_name) " yet" } },
            }
        }
    }
}

fn describe_tag_choice(
    shown: &Shown,
    resolved: Option<&Result<crate::kubernetes::parameters::ParameterValue, String>>,
) -> Markup {
    match shown.kind {
        "pin" if shown.typed.trim().is_empty() => html! { span.muted { "type a tag" } },
        "pin" => html! { "resolves to " strong.mono { (shown.typed.trim()) } },
        "track" if shown.typed.trim().is_empty() => {
            html! { span.muted { "type a semver range, such as 1.27.*" } }
        }
        _ => match resolved {
            Some(Ok(value)) => html! { "resolves to " strong.mono { (value.rendered()) } },
            Some(Err(_)) => html! { span.muted { "cannot resolve; see the preview" } },
            None => html! {},
        },
    }
}

fn render_parameter_selector(
    conn: &PooledConnection<SqliteConnectionManager>,
    config: &DeployConfig,
    prepared: &Prepared,
    resolved: &TagResolutions,
    name: &str,
    source: &ParameterSource,
) -> Markup {
    let shown = shown_choice(config, &prepared.action, name);
    let sel_name = format!("sel_{name}");
    let radio = |kind: &str, label: Markup| {
        html! {
            label.param-selector__opt {
                input type="radio" form=(ACTION_FORM) name=(sel_name) value=(kind) checked[shown.kind == kind] onchange="this.form.submit()";
                span { (label) }
            }
        }
    };
    let text = |kind: &str| {
        html! {
            input.param-selector__input type="text" form=(ACTION_FORM) name=(format!("{kind}_{name}")) value=(shown.typed) onblur="this.form.submit()" autocomplete="off" spellcheck="false" autofocus[shown.typed.is_empty()];
        }
    };
    let current = config.selection(name);
    html! {
        div.param-selector {
            div.param-selector__head {
                code { "$" (name) }
                span.muted {
                    (source.type_name())
                    @match source {
                        ParameterSource::Commit { owner, repo, .. } => { " · " (owner) "/" (repo) }
                        ParameterSource::Tag { image, .. } => { " · " (image) }
                        ParameterSource::Value { .. } => {}
                    }
                }
            }
            @match source {
                ParameterSource::Commit { branch, .. } => {
                    (radio("default", html! { "Track default (" span.mono { (branch) } ")" }))
                    (radio("track", html! { "Track another branch" }))
                    @if shown.kind == "track" { (text("track")) }
                    (radio("pin", html! { "Pin an exact SHA" }))
                    @if shown.kind == "pin" { (text("pin")) }
                    div.param-selector__resolves {
                        (describe_commit_choice(conn, config, &shown, branch))
                    }
                }
                ParameterSource::Tag {
                    pattern, variant, ..
                } => {
                    (radio("default", html! { "Track default (" span.mono { (ParameterSource::channel_label(pattern, variant.as_deref())) } ")" }))
                    (radio("track", html! { "Track another range" }))
                    @if shown.kind == "track" { (text("track")) }
                    (radio("pin", html! { "Pin an exact tag" }))
                    @if shown.kind == "pin" { (text("pin")) }
                    div.param-selector__resolves {
                        (describe_tag_choice(&shown, resolved.get(name)))
                    }
                }
                ParameterSource::Value { default } => {
                    (radio("default", html! { "Track default (" span.mono { (default) } ")" }))
                    (radio("pin", html! { "Pin a value" }))
                    @if shown.kind == "pin" { (text("pin")) }
                    @if shown.kind == "pin" && !shown.typed.trim().is_empty() {
                        div.param-selector__resolves { "resolves to " strong.mono { (shown.typed.trim()) } }
                    } @else if shown.kind == "pin" {
                        div.param-selector__resolves { span.muted { "type a value" } }
                    }
                }
            }
            @if current.is_override() && shown.kind != "default" && prepared.action.choices().is_none_or(|c| !c.contains_key(name)) {
                div.param-selector__current {
                    (render_durability_badge(current.durability))
                    @if let Some(note) = &current.note { span.faint { " · " (note) } }
                    @if let Some(since) = current.since.as_deref().and_then(formatting::rfc3339_to_ms) {
                        span.faint { " · since " (formatting::format_ago_short(since)) }
                    }
                }
            }
        }
    }
}

fn render_durability_toggle(durability: Durability) -> Markup {
    let option = |value: Durability, label: &str| {
        html! {
            label class=(format!("durability-toggle__opt durability-toggle__opt--{}", value.as_str())) {
                input type="radio" form=(ACTION_FORM) name="durability" value=(value.as_str()) checked[durability == value] onchange="this.form.submit()";
                span { (label) }
            }
        }
    };
    html! {
        div.durability-toggle {
            (option(Durability::Temporary, "Temporary"))
            (option(Durability::Standing, "Standing"))
        }
    }
}

/// The whole left column.
pub fn render(
    config: &DeployConfig,
    sorted_configs: &[DeployConfig],
    prepared: &Prepared,
    query: &HashMap<String, String>,
    held: bool,
    resolved: &TagResolutions,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Markup {
    let action = &prepared.action;
    let name = config.name_any();
    let is_orphaned = config.is_orphaned();
    let advanced = action.is_deploy_advanced();
    let durability = action.durability().unwrap_or(Durability::Temporary);
    let deploy_disabled = is_orphaned || held;
    let button_disabled =
        (is_orphaned && !action.is_undeploy()) || (held && action.is_gated_by_blockers());
    let mut parameter_names: Vec<&String> = config.spec.spec.parameters.keys().collect();
    parameter_names.sort_by_key(|n| (n.as_str() != SHA_PARAMETER, n.as_str()));
    let radio = |value: &str, checked: bool, disabled: bool, label: Markup| {
        html! {
            label.action-radio.action-radio--disabled[disabled] {
                input type="radio" form=(ACTION_FORM) name="action" value=(value) checked[checked] disabled[disabled] onchange="this.form.submit()";
                span { (label) }
            }
        }
    };

    html! {
        div class="left-box" {
            h3 { "Deploy config" }
            form action="/deploy" method="get" {
                div.config-picker {
                    select name="selected" onchange="this.form.submit()" aria-label="Deploy config" {
                        @for candidate in sorted_configs {
                            @let candidate_name = candidate.name_any();
                            option value=(candidate_name) selected[candidate_name == name] { (candidate_name) }
                        }
                    }
                    @if config.is_temporary_deployment() {
                        (render_durability_badge(Durability::Temporary))
                    }
                }
            }

            form id=(ACTION_FORM) action="/deploy" method="get" {
                input type="hidden" name="selected" value=(name);
            }

            h3 { "Action" }
            div.action-radio-group {
                @if config.is_temporary_deployment() && !is_orphaned {
                    (radio("end-temporary", action.is_end_temporary(), held, html! { "End temporary deployment" }))
                }
                (radio("deploy", action.is_deploy() && !advanced, deploy_disabled, html! { "Deploy" }))
                (radio("deploy-advanced", advanced, deploy_disabled, html! { "Deploy advanced" }))
                (radio("redeploy-previous", action.is_redeploy_previous(), deploy_disabled, html! { "Redeploy previous" }))
                (radio("toggle-autodeploy", action.is_toggle_autodeploy(), is_orphaned, html! {
                    @if config.autodeploy() { "Disable autodeploy" } @else { "Enable autodeploy" }
                }))
                @if config.supports_bounce() {
                    (radio("bounce", action.is_bounce(), is_orphaned, html! { "Bounce" }))
                }
                @if config.supports_execute_job() {
                    (radio("execute-job", action.is_execute_job(), is_orphaned, html! { "Execute job" }))
                }
                (radio("undeploy", action.is_undeploy(), false, html! { "Undeploy" }))
            }

            @if advanced && !is_orphaned {
                h3 { "Parameters" }
                @for pname in parameter_names.iter().copied() {
                    @if let Some(source) = config.spec.spec.parameters.get(pname) {
                        (render_parameter_selector(conn, config, prepared, resolved, pname, source))
                    }
                }
                @if config.spec.spec.parameters.is_empty() {
                    p.muted { "None declared." }
                }

                h3 { "Patches" }
                (crate::web::patches::render_patch_list(config, action))

                h3 { "Durability" }
                (render_durability_toggle(durability))
            }

            form id="deployForm" action=(format!("/api/deploy/{}/{}", config.namespace().unwrap_or_default(), name)) method="post"
                onsubmit="const b=this.querySelector('button[type=submit]'); if (b) { b.disabled=true; b.textContent='Working…'; }" {
                input type="hidden" name="action" value=(action.form_value());
                @if let Some(revision) = action.replayed_revision() {
                    input type="hidden" name="revision" value=(revision);
                }
                @if let Some(choices) = action.choices() {
                    input type="hidden" name="durability" value=(durability.as_str());
                    @if let Some(pending) = action.patch_changes().filter(|p| !p.is_empty()) {
                        input type="hidden" name="patches" value=(pending.to_json());
                    }
                    @for (pname, choice) in choices {
                        input type="hidden" name=(format!("sel_{pname}")) value=(choice.kind());
                        @if let Some(typed) = choice.typed() {
                            input type="hidden" name=(format!("{}_{pname}", choice.kind())) value=(typed);
                        }
                    }
                } @else {
                    input type="hidden" name="branch" value=(query.get("branch").map(String::as_str).unwrap_or(""));
                    input type="hidden" name="sha" value=(query.get("sha").map(String::as_str).unwrap_or(""));
                    input type="hidden" name="durability" value=(query.get("durability").map(String::as_str).unwrap_or(""));
                }
                button.primary-action-button.danger-button[action.is_undeploy()] type="submit" disabled[button_disabled] {
                    (action.button_label(config))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::selections::{Choice, Selection};
    use std::collections::BTreeMap;

    fn config() -> DeployConfig {
        use crate::kubernetes::deploy_config::{DeployConfigSpec, DeployConfigSpecFields};
        use crate::kubernetes::repo::Repository;
        let mut dc = DeployConfig::new(
            "site",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters: ParameterSource::sha_map(Some(
                        Repository {
                            owner: "o".into(),
                            repo: "r".into(),
                        }
                        .with_branch("master"),
                    )),
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "o".into(),
                        repo: "c".into(),
                    },
                    specs: vec![],
                },
            },
        );
        dc.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::pin("1.27.4", Durability::Standing),
        );
        dc
    }

    #[test]
    fn the_form_shows_the_choice_it_sent_or_else_the_current_selection() {
        let config = config();
        let plain = Action::DeployAdvanced {
            choices: BTreeMap::new(),
            durability: Durability::Temporary,
            patches: Default::default(),
        };
        let sha = shown_choice(&config, &plain, SHA_PARAMETER);
        assert_eq!((sha.kind, sha.typed.as_str()), ("default", ""));
        let nginx = shown_choice(&config, &plain, "NGINX");
        assert_eq!((nginx.kind, nginx.typed.as_str()), ("pin", "1.27.4"));

        let sent = Action::DeployAdvanced {
            choices: BTreeMap::from([("NGINX".to_string(), Choice::Track("1.28.*".into()))]),
            durability: Durability::Temporary,
            patches: Default::default(),
        };
        let nginx = shown_choice(&config, &sent, "NGINX");
        assert_eq!((nginx.kind, nginx.typed.as_str()), ("track", "1.28.*"));
    }
}
