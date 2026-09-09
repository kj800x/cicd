//! The preview column of the deploy page.
//!
//! One row per parameter, `NAME: current → new (channel)`, with `CONFIG`
//! first: the config commit is recorded with every deploy but is not a
//! form parameter. Rows that do not change print one value and drop to the
//! faint colour; rows that do keep the arrow and bold the new value. A tag
//! that watchtower cannot resolve is the one row that grows a control: a
//! box to type the tag for this deploy only.
//!
//! Above the rows sit the alerts (held, temporary, build state, cluster
//! state); below them the patches that will be active and the resources
//! the config owns. Everything inside [`render_preview_content`] is polled.

use std::collections::BTreeMap;

use kube::api::DynamicObject;
use kube::{Client, ResourceExt};
use maud::{html, Markup};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

use crate::crab_ext::Octocrabs;
use crate::db::blocker::Blocker;
use crate::db::deploy_config_version::DeployConfigVersion;
use crate::db::git_repo::GitRepo;
use crate::db::revision::Revision;
use crate::deploys::{normalize_action, resolve_tag_parameter};
use crate::error::AppResult;
use crate::kubernetes::api::get_namespace_uid;
use crate::kubernetes::parameters::{ParameterSource, ParameterValue, SHA_PARAMETER};
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::selections::{Durability, Mode};
use crate::kubernetes::DeployConfig;
use crate::watchtower::Watchtower;
use crate::web::resource_status::ResourcePlan;
use crate::web::{build_status, deploy_status, Action, ResourceStatuses};

// FIXME: Make this configurable.
const HEADLAMP_URL: &str = "https://headlamp.home.coolkev.com";

/// An action resolved against a config: the action with any abbreviated
/// sha expanded, and the config as the action would leave its selections
/// and patches. The form and the preview both work from this so they
/// agree.
pub struct Prepared {
    pub config: DeployConfig,
    pub action: Action,
    pub effective: DeployConfig,
    /// Why the action could not be normalized (an ambiguous sha, say).
    /// The action is kept as typed so the page still renders.
    pub problem: Option<String>,
}

pub fn prepare(
    conn: &PooledConnection<SqliteConnectionManager>,
    config: &DeployConfig,
    action: &Action,
) -> Prepared {
    let (action, problem) = match normalize_action(conn, config, action) {
        Ok(normalized) => (normalized, None),
        Err(e) => (action.clone(), Some(e.to_string())),
    };
    let effective = action.effective_config(config);
    Prepared {
        config: config.clone(),
        action,
        effective,
        problem,
    }
}

/// What each tag parameter resolves to for this deploy, or why it cannot.
pub type TagResolutions = BTreeMap<String, Result<ParameterValue, String>>;

/// Ask watchtower about every tag parameter, once per render. Only actions
/// that deploy something new need it; undeploys, rollbacks and the
/// non-version actions leave tags alone.
pub async fn resolve_tags(prepared: &Prepared, typed: &BTreeMap<String, String>) -> TagResolutions {
    let mut out = TagResolutions::new();
    if !resolves_new_values(&prepared.action) {
        return out;
    }
    for (pname, source) in &prepared.effective.spec.spec.parameters {
        if !source.is_tag() {
            continue;
        }
        let result = resolve_tag_parameter(
            Watchtower::global(),
            &prepared.effective,
            pname,
            typed.get(pname).map(String::as_str),
        )
        .await;
        match result {
            Ok(Some(value)) => {
                out.insert(pname.clone(), Ok(value));
            }
            Ok(None) => {}
            Err(e) => {
                out.insert(pname.clone(), Err(e.to_string()));
            }
        }
    }
    out
}

/// Actions whose new parameter values come from resolving channels now.
fn resolves_new_values(action: &Action) -> bool {
    action.changes_deployment() && !matches!(action, Action::Undeploy | Action::Rollback { .. })
}

/// The heading, the meta row and the polled body.
pub async fn render_page_preview(
    prepared: &Prepared,
    conn: &PooledConnection<SqliteConnectionManager>,
    client: &Client,
    octocrabs: Option<&Octocrabs>,
    namespaced_objs: &[DynamicObject],
    typed: &BTreeMap<String, String>,
    resolved: &TagResolutions,
) -> Markup {
    let config = &prepared.config;
    let name = config.name_any();
    let namespace = config.namespace().unwrap_or("default".to_string());
    let namespace_uid = get_namespace_uid(client, &namespace)
        .await
        .unwrap_or_default();
    let mut poll_url = format!(
        "/fragments/deploy-preview/{}/{}?{}",
        namespace,
        name,
        prepared.action.as_params()
    );
    for (parameter, value) in typed {
        poll_url.push_str(&format!(
            "&value_{}={}",
            parameter,
            url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
        ));
    }
    html! {
        div class="right-box" {
            h1 {
                (prepared.action.title())
                strong { (name) }
            }
            div class="preview-meta" {
                span {
                    "Namespace "
                    a.mono href=(format!("{}/c/main/map?group=namespace&node={}", HEADLAMP_URL, namespace_uid)) target="_blank" title="Open in Headlamp" { (namespace) }
                }
                span.preview-meta__item {
                    "Autodeploy "
                    (render_autodeploy_badge(config.autodeploy()))
                }
            }
            div.preview-content-poll-wrapper
                hx-get=(poll_url)
                hx-include="[name^=value_]"
                hx-trigger="load, every 2s"
                hx-swap="morph:{morphStyle:'innerHTML',ignoreActiveValue:true}" {
                (render_preview_content(prepared, conn, octocrabs, namespaced_objs, typed, resolved).await)
            }
        }
    }
}

pub fn render_autodeploy_badge(enabled: bool) -> Markup {
    if enabled {
        html! { span.hl-badge.hl-badge--ok { "Enabled" } }
    } else {
        html! { span.hl-badge.hl-badge--fail { "Disabled" } }
    }
}

pub fn render_durability_badge(durability: Durability) -> Markup {
    match durability {
        Durability::Temporary => html! { span.hl-badge.hl-badge--temporary { "temporary" } },
        Durability::Standing => html! { span.hl-badge.hl-badge--standing { "standing" } },
    }
}

/// A value shown in a row: shortened for display, complete on the title,
/// and linked when it names something on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    short: String,
    full: String,
    href: Option<String>,
}

impl Shown {
    fn plain(value: &str) -> Self {
        Shown {
            short: value.to_string(),
            full: value.to_string(),
            href: None,
        }
    }

    fn commit(owner: &str, repo: &str, sha: &str, path: Option<&str>) -> Self {
        Shown {
            short: crate::web::formatting::format_short_sha(sha).to_string(),
            full: sha.to_string(),
            href: Some(format!(
                "https://github.com/{}/{}/tree/{}{}",
                owner,
                repo,
                sha,
                path.map(|p| format!("/{p}")).unwrap_or_default()
            )),
        }
    }

    fn render(&self, class: &str) -> Markup {
        match &self.href {
            Some(href) => {
                html! { a class=(class) href=(href) target="_blank" title=(self.full) { (self.short) } }
            }
            None => html! { span class=(class) title=(self.full) { (self.short) } },
        }
    }
}

/// What a row moves to.
enum Target {
    /// A value, or `None` for "Undeployed".
    Value(Option<Shown>),
    /// No value could be worked out; the message says why.
    Unresolved(String),
}

/// The box to type a tag for this deploy only. Rendered whenever a tag
/// cannot be resolved, and kept while a typed value is in play so the
/// value survives the poll and reaches the deploy form.
struct OneShot {
    parameter: String,
    typed: Option<String>,
    placeholder: String,
}

struct ParamRow {
    name: String,
    from: Option<Shown>,
    to: Target,
    channel: Option<String>,
    badge: Option<Durability>,
    compare: Option<String>,
    marker: Option<&'static str>,
    note: Option<String>,
    one_shot: Option<OneShot>,
}

impl ParamRow {
    fn is_unchanged(&self) -> bool {
        match &self.to {
            Target::Value(to) => same(self.from.as_ref(), to.as_ref()),
            Target::Unresolved(_) => false,
        }
    }

    fn is_unresolved(&self) -> bool {
        matches!(self.to, Target::Unresolved(_))
    }
}

fn same(a: Option<&Shown>, b: Option<&Shown>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.full == b.full,
        _ => false,
    }
}

fn compare_url(
    owner: &str,
    repo: &str,
    from: Option<&Shown>,
    to: Option<&Shown>,
) -> Option<String> {
    match (from, to) {
        (Some(from), Some(to)) if from.full != to.full && from.href.is_some() => Some(format!(
            "https://github.com/{}/{}/compare/{}...{}",
            owner, repo, from.full, to.full
        )),
        _ => None,
    }
}

fn config_sha(state: &DeploymentState) -> Option<&str> {
    match state {
        DeploymentState::DeployedWithArtifact { config, .. }
        | DeploymentState::DeployedOnlyConfig { config } => Some(config.sha.as_str()),
        DeploymentState::Undeployed => None,
    }
}

fn config_branch(state: &DeploymentState) -> Option<&str> {
    match state {
        DeploymentState::DeployedWithArtifact { config, .. }
        | DeploymentState::DeployedOnlyConfig { config } => config.branch.as_deref(),
        DeploymentState::Undeployed => None,
    }
}

fn artifact_sha(state: &DeploymentState) -> Option<&str> {
    match state {
        DeploymentState::DeployedWithArtifact { artifact, .. } => Some(artifact.sha.as_str()),
        _ => None,
    }
}

/// Whether the config manifests differ between two config commits, when
/// both hashes are known.
fn manifests_changed(
    conn: &PooledConnection<SqliteConnectionManager>,
    config: &DeployConfig,
    from: Option<&str>,
    to: Option<&str>,
) -> Option<bool> {
    let cfg_repo = config.config_repository();
    let repo_id = GitRepo::get_by_name(&cfg_repo.owner, &cfg_repo.repo, conn)
        .ok()??
        .id;
    let name = config.name_any();
    let hash = |sha: Option<&str>| {
        sha.and_then(|sha| {
            DeployConfigVersion::get_hash(&name, repo_id, sha, conn)
                .ok()
                .flatten()
        })
    };
    match (hash(from), hash(to)) {
        (Some(fh), Some(th)) => Some(fh != th),
        _ => None,
    }
}

/// Every row, `CONFIG` first, then `SHA`, then the rest by name.
fn parameter_rows(
    prepared: &Prepared,
    conn: &PooledConnection<SqliteConnectionManager>,
    from: &DeploymentState,
    to: &AppResult<DeploymentState>,
    typed: &BTreeMap<String, String>,
    resolved: &TagResolutions,
) -> Vec<ParamRow> {
    let config = &prepared.config;
    let effective = &prepared.effective;
    let action = &prepared.action;
    let name = config.name_any();
    let deploys = action.changes_deployment();
    let undeploys = matches!(action, Action::Undeploy);
    let revision = match action {
        Action::Rollback { revision } => Revision::get(conn, *revision).ok().flatten(),
        _ => None,
    };
    let deployed = config
        .status
        .as_ref()
        .map(|s| s.parameters.clone())
        .unwrap_or_default();
    let is_deployed = !matches!(from, DeploymentState::Undeployed);

    let mut rows = Vec::new();

    // CONFIG
    {
        let cfg_repo = config.config_repository();
        let path = format!(".deploy/{name}");
        let shown = |sha: &str| Shown::commit(&cfg_repo.owner, &cfg_repo.repo, sha, Some(&path));
        let from_shown = config_sha(from).map(shown);
        let (to_target, branch) = if !deploys {
            (Target::Value(from_shown.clone()), config_branch(from))
        } else {
            match to {
                Ok(state) => (
                    Target::Value(config_sha(state).map(shown)),
                    config_branch(state),
                ),
                Err(e) => (Target::Unresolved(e.to_string()), config_branch(from)),
            }
        };
        let to_shown = match &to_target {
            Target::Value(v) => v.clone(),
            Target::Unresolved(_) => None,
        };
        let changed = !same(from_shown.as_ref(), to_shown.as_ref());
        let marker = if changed
            && manifests_changed(
                conn,
                config,
                from_shown.as_ref().map(|s| s.full.as_str()),
                to_shown.as_ref().map(|s| s.full.as_str()),
            ) == Some(true)
        {
            Some("[CONFIG CHANGED]")
        } else {
            None
        };
        rows.push(ParamRow {
            name: "CONFIG".to_string(),
            compare: compare_url(
                &cfg_repo.owner,
                &cfg_repo.repo,
                from_shown.as_ref(),
                to_shown.as_ref(),
            ),
            from: from_shown,
            to: to_target,
            channel: branch.map(String::from),
            badge: None,
            marker,
            note: None,
            one_shot: None,
        });
    }

    let mut names: Vec<&String> = effective.spec.spec.parameters.keys().collect();
    names.sort_by_key(|n| (n.as_str() != SHA_PARAMETER, n.as_str()));
    for pname in names {
        let Some(source) = effective.spec.spec.parameters.get(pname) else {
            continue;
        };
        let selection = if pname == SHA_PARAMETER {
            action.effective_sha_selection(effective)
        } else {
            effective.selection(pname)
        };
        let badge = selection.is_override().then_some(selection.durability);
        let note = selection.note.clone();
        let revision_value = revision
            .as_ref()
            .and_then(|r| r.parameter(pname))
            .map(|p| p.value.clone());
        match source {
            ParameterSource::Commit {
                owner,
                repo,
                branch,
            } if pname == SHA_PARAMETER => {
                let shown = |sha: &str| Shown::commit(owner, repo, sha, None);
                let from_shown = artifact_sha(from).map(shown);
                let to_target = if !deploys {
                    Target::Value(from_shown.clone())
                } else {
                    match to {
                        Ok(state) => Target::Value(artifact_sha(state).map(shown)),
                        Err(e) => Target::Unresolved(e.to_string()),
                    }
                };
                let to_shown = match &to_target {
                    Target::Value(v) => v.clone(),
                    Target::Unresolved(_) => None,
                };
                let channel = if undeploys {
                    None
                } else if let Some(rev) = &revision {
                    Some(format!("revision {}", rev.id))
                } else {
                    Some(match selection.mode() {
                        Mode::Default => format!("tracking {branch}"),
                        Mode::Track(b) => format!("tracking {b}"),
                        Mode::Pin(_) => "pinned".to_string(),
                    })
                };
                rows.push(ParamRow {
                    name: pname.clone(),
                    compare: compare_url(owner, repo, from_shown.as_ref(), to_shown.as_ref()),
                    from: from_shown,
                    to: to_target,
                    channel,
                    badge,
                    marker: None,
                    note,
                    one_shot: None,
                });
            }
            ParameterSource::Commit { owner, repo, .. } => {
                // A commit parameter other than SHA is deployed as recorded;
                // nothing resolves it, so it is shown as it stands.
                let value = deployed.get(pname);
                let from_shown = value.map(|v| Shown::commit(owner, repo, &v.rendered(), None));
                let to_target = if undeploys {
                    Target::Value(None)
                } else {
                    Target::Value(from_shown.clone())
                };
                rows.push(ParamRow {
                    name: pname.clone(),
                    from: from_shown,
                    to: to_target,
                    channel: value
                        .and_then(|v| v.channel())
                        .map(|b| format!("tracking {b}")),
                    badge,
                    compare: None,
                    marker: None,
                    note,
                    one_shot: None,
                });
            }
            ParameterSource::Tag { pattern, .. } => {
                let from_shown = deployed.get(pname).map(|v| Shown::plain(&v.rendered()));
                let typed_value = typed.get(pname).cloned();
                let to_target = if undeploys {
                    Target::Value(None)
                } else if revision.is_some() {
                    match &revision_value {
                        Some(v) => Target::Value(Some(Shown::plain(v))),
                        None => Target::Unresolved("the revision recorded no value".to_string()),
                    }
                } else if !deploys {
                    Target::Value(from_shown.clone())
                } else {
                    match resolved.get(pname) {
                        Some(Ok(value)) => Target::Value(Some(Shown::plain(&value.rendered()))),
                        Some(Err(message)) => Target::Unresolved(message.clone()),
                        None => Target::Value(from_shown.clone()),
                    }
                };
                let channel = if undeploys {
                    None
                } else if let Some(rev) = &revision {
                    Some(format!("revision {}", rev.id))
                } else {
                    Some(match selection.mode() {
                        Mode::Default => pattern.clone(),
                        Mode::Track(p) => p.to_string(),
                        Mode::Pin(_) => "pinned".to_string(),
                    })
                };
                let needs_input = resolves_new_values(action)
                    && !matches!(selection.mode(), Mode::Pin(_))
                    && (typed_value.is_some() || matches!(to_target, Target::Unresolved(_)));
                let one_shot = needs_input.then(|| OneShot {
                    parameter: pname.clone(),
                    typed: typed_value,
                    placeholder: from_shown
                        .as_ref()
                        .map(|s| s.full.clone())
                        .unwrap_or_else(|| pattern.clone()),
                });
                rows.push(ParamRow {
                    name: pname.clone(),
                    from: from_shown,
                    to: to_target,
                    channel,
                    badge,
                    compare: None,
                    marker: None,
                    note,
                    one_shot,
                });
            }
            ParameterSource::Value { default } => {
                let from_shown = match deployed.get(pname) {
                    Some(v) => Some(Shown::plain(&v.rendered())),
                    None if is_deployed => Some(Shown::plain(default)),
                    None => None,
                };
                let to_target = if undeploys {
                    Target::Value(None)
                } else if revision.is_some() {
                    match &revision_value {
                        Some(v) => Target::Value(Some(Shown::plain(v))),
                        None => Target::Value(Some(Shown::plain(default))),
                    }
                } else if !deploys {
                    Target::Value(from_shown.clone())
                } else {
                    let value = match selection.mode() {
                        Mode::Pin(v) => v.to_string(),
                        _ => default.clone(),
                    };
                    Target::Value(Some(Shown::plain(&value)))
                };
                let channel = if undeploys {
                    None
                } else if let Some(rev) = &revision {
                    Some(format!("revision {}", rev.id))
                } else {
                    Some(match selection.mode() {
                        Mode::Pin(_) => "pinned".to_string(),
                        _ => "default".to_string(),
                    })
                };
                rows.push(ParamRow {
                    name: pname.clone(),
                    from: from_shown,
                    to: to_target,
                    channel,
                    badge,
                    compare: None,
                    marker: None,
                    note,
                    one_shot: None,
                });
            }
        }
    }
    rows
}

fn render_undeployed(is_target: bool) -> Markup {
    html! { span.param-undeployed.param-undeployed--new[is_target] { "Undeployed" } }
}

fn render_row(row: &ParamRow) -> Markup {
    let unchanged = row.is_unchanged();
    let unresolved = row.is_unresolved();
    html! {
        div.param-row.param-row--same[unchanged].param-row--unresolved[unresolved] {
            span.param-row__name { (row.name) ":" }
            span.param-row__value {
                @match &row.to {
                    Target::Value(to) if unchanged => {
                        @match &row.from {
                            Some(from) => (from.render("param-value")),
                            None => (render_undeployed(false)),
                        }
                        @let _ = to;
                    }
                    Target::Value(to) => {
                        @match &row.from {
                            Some(from) => (from.render("param-old")),
                            None => (render_undeployed(false)),
                        }
                        span.param-arrow { "→" }
                        @match to {
                            Some(to) => (to.render("param-new")),
                            None => (render_undeployed(true)),
                        }
                    }
                    Target::Unresolved(_) => {
                        @match &row.from {
                            Some(from) => (from.render("param-old")),
                            None => (render_undeployed(false)),
                        }
                        span.param-arrow { "→" }
                        strong.param-unresolved { "cannot resolve" }
                    }
                }
                @if let Some(channel) = &row.channel {
                    span.param-channel { "(" (channel) ")" }
                }
                @if let Some(url) = &row.compare {
                    a.param-compare href=(url) target="_blank" { "[compare]" }
                }
                @if let Some(durability) = row.badge {
                    span.param-badge { (render_durability_badge(durability)) }
                }
                @if let Some(marker) = row.marker {
                    span.param-marker { (marker) }
                }
                @if let Some(note) = &row.note {
                    span.param-note-inline { "· " (note) }
                }
                @if let Target::Unresolved(message) = &row.to {
                    div.param-problem { (message) }
                }
                @if let Some(one_shot) = &row.one_shot {
                    div.param-oneshot {
                        input type="text" form="deployForm" name=(format!("value_{}", one_shot.parameter)) value=[one_shot.typed.as_deref()] placeholder=(one_shot.placeholder) aria-label=(format!("{} for this deploy", one_shot.parameter)) autocomplete="off";
                        span.param-oneshot__hint {
                            @if one_shot.typed.is_some() {
                                "typed for this deploy · not saved as an override"
                            } @else {
                                "type a tag for this deploy · not saved as an override"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_alert(tone: &str, title: Markup, body: Markup) -> Markup {
    html! {
        div class=(format!("alert alert-{tone}")) {
            div class="alert-header" { (title) }
            div class="alert-content" { div class="details" { (body) } }
        }
    }
}

fn render_strip(tone: &str, body: Markup) -> Markup {
    html! {
        div class=(format!("preview-strip preview-strip--{tone}")) { (body) }
    }
}

/// `(kind, name)` of every manifest in a config's templates.
fn manifest_names(config: &DeployConfig) -> Vec<(String, String)> {
    config
        .resource_specs()
        .iter()
        .filter_map(|m| {
            let kind = m.get("kind")?.as_str()?.to_string();
            let name = m.get("metadata")?.get("name")?.as_str()?.to_string();
            Some((kind, name))
        })
        .collect()
}

/// What the deploy does to the resource tree. An undeploy removes
/// everything. A deploy that moves the config commit reads the manifests
/// at that commit (through the per-commit memo, so one GitHub fetch serves
/// every poll) and diffs them against what is deployed.
async fn resource_plan(
    prepared: &Prepared,
    octocrabs: Option<&Octocrabs>,
    from: &DeploymentState,
    to: &AppResult<DeploymentState>,
) -> ResourcePlan {
    let config = &prepared.config;
    let current = manifest_names(config);
    if matches!(prepared.action, Action::Undeploy) {
        return ResourcePlan {
            removed: current,
            created: vec![],
        };
    }
    if !prepared.action.changes_deployment() {
        return ResourcePlan::default();
    }
    let Ok(to) = to else {
        return ResourcePlan::default();
    };
    let (Some(target_sha), Some(octocrabs)) = (config_sha(to), octocrabs) else {
        return ResourcePlan::default();
    };
    if config_sha(from) == Some(target_sha) {
        return ResourcePlan::default();
    }
    let desired = match crate::webhooks::config_sync::fetch_deploy_config_cached(
        octocrabs,
        config.config_repository(),
        target_sha,
        &config.name_any(),
    )
    .await
    {
        Ok(Some(desired)) => manifest_names(&desired),
        Ok(None) => vec![],
        Err(e) => {
            log::warn!(
                "Could not read {} at {} for the preview: {}",
                config.name_any(),
                target_sha,
                e
            );
            return ResourcePlan::default();
        }
    };
    ResourcePlan {
        removed: current
            .iter()
            .filter(|c| !desired.contains(c))
            .cloned()
            .collect(),
        created: desired
            .into_iter()
            .filter(|d| !current.contains(d))
            .collect(),
    }
}

/// The polled part of the preview: alerts, parameter rows, patches, the
/// result line and the resource tree.
pub async fn render_preview_content(
    prepared: &Prepared,
    conn: &PooledConnection<SqliteConnectionManager>,
    octocrabs: Option<&Octocrabs>,
    namespaced_objs: &[DynamicObject],
    typed: &BTreeMap<String, String>,
    resolved: &TagResolutions,
) -> Markup {
    let config = &prepared.config;
    let effective = &prepared.effective;
    let action = &prepared.action;
    let name = config.name_any();

    let from = config.deployment_state();
    let to = DeploymentState::from_action(action, effective, conn);
    let rows = parameter_rows(prepared, conn, &from, &to, typed, resolved);
    let plan = resource_plan(prepared, octocrabs, &from, &to).await;

    let mut alerts: Vec<Markup> = Vec::new();
    if let Some(problem) = &prepared.problem {
        alerts.push(render_alert(
            "danger",
            html! { "Cannot resolve this action" },
            html! { (problem) },
        ));
    }
    if config.is_orphaned() {
        alerts.push(render_alert(
            "neutral",
            html! { "Orphaned" },
            html! { "The manifests are gone from the config repo. Only undeploy is offered." },
        ));
    }
    for alert in deploy_status(config, namespaced_objs).await {
        alerts.push(alert);
    }
    if config.artifact_repository().is_some() && action.changes_deployment() {
        for alert in build_status(action, effective, conn).await {
            alerts.push(alert);
        }
    }
    let mut held = false;
    match Blocker::active_for(conn, &name) {
        Ok(blockers) if !blockers.is_empty() => {
            held = true;
            alerts.push(crate::web::blockers::render_blocker_alert(&blockers))
        }
        Ok(_) => {}
        Err(e) => log::warn!("Failed to load blockers for preview: {}", e),
    }
    let already_temporary = config.is_temporary_deployment();
    if already_temporary {
        alerts.push(crate::web::selections::render_temporary_alert(config));
    }
    let deploys_new = resolves_new_values(action);
    if deploys_new && !already_temporary && effective.has_temporary_overrides() {
        let overrides = temporary_override_count(effective);
        let patches = effective
            .spec
            .spec
            .patches
            .iter()
            .filter(|p| p.is_temporary())
            .count();
        alerts.push(render_alert(
            "warning",
            html! { i class="fa fa-flask" {} " This deploy becomes a temporary deployment" },
            html! {
                (plural(overrides, "temporary override")) " · " (plural(patches, "temporary patch"))
                ". Autodeploy is suppressed while a temporary deployment is active."
            },
        ));
    }

    // What a non-version action does, in one line.
    let action_line = match action {
        Action::ToggleAutodeploy => Some(render_strip(
            "neutral",
            html! {
                "Autodeploy "
                (render_autodeploy_badge(config.autodeploy()))
                span.param-arrow { "→" }
                (render_autodeploy_badge(!config.autodeploy()))
            },
        )),
        Action::Bounce => Some(render_strip(
            "neutral",
            html! { "Restarts every Deployment this config owns." },
        )),
        Action::ExecuteJob => Some(render_strip(
            "neutral",
            html! { "Creates a Job from each CronJob this config owns." },
        )),
        Action::Undeploy if matches!(from, DeploymentState::Undeployed) => {
            Some(render_strip("neutral", html! { "Already undeployed." }))
        }
        _ => None,
    };

    // Patches: what stays, what goes, what this deploy adds.
    let ending = action.is_end_temporary();
    let pending = action.patch_changes();
    let deploy_durability = action.durability().unwrap_or(Durability::Temporary);
    let mut patch_lines: Vec<(&str, crate::kubernetes::patches::ManifestPatch)> = config
        .spec
        .spec
        .patches
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if (ending && p.is_temporary()) || pending.is_some_and(|c| c.removes(i)) {
                ("removed", p.clone())
            } else {
                ("kept", p.clone())
            }
        })
        .collect();
    if let Some(pending) = pending {
        for p in &pending.add {
            let mut p = p.clone();
            p.durability = deploy_durability;
            patch_lines.push(("added", p));
        }
    }
    let patches_change = patch_lines.iter().any(|(status, _)| *status != "kept");

    let at_latest = deploys_new
        && prepared.problem.is_none()
        && to.is_ok()
        && rows.iter().all(ParamRow::is_unchanged)
        && !patches_change
        && plan.removed.is_empty()
        && plan.created.is_empty();
    if at_latest {
        alerts.push(render_strip("neutral", html! { "Already at latest." }));
    }

    let overrides = override_count(effective);
    let temporary_patches = effective
        .spec
        .spec
        .patches
        .iter()
        .filter(|p| p.is_temporary())
        .count();
    let would_be_temporary = effective.has_temporary_overrides();

    html! {
        @for alert in alerts {
            (alert)
        }
        @if let Some(line) = action_line {
            (line)
        }
        div.eyebrow { "Parameters" }
        @let refused = held && action.is_gated_by_blockers();
        div.param-rows.param-rows--dimmed[!action.changes_deployment() || refused] {
            @for row in &rows {
                (render_row(row))
            }
        }
        @if !patch_lines.is_empty() {
            div.eyebrow {
                @if action.changes_deployment() && !matches!(action, Action::Undeploy) {
                    "Patches that will be active"
                } @else {
                    "Patches"
                }
            }
            div.patch-lines {
                @for (status, patch) in &patch_lines {
                    div.patch-line {
                        span class=(format!("patch-line__status patch-line__status--{status}")) { (status) }
                        code.patch-line__op { (patch.describe_op()) }
                        span class=(format!("patch-line__durability patch-line__durability--{}", patch.durability.as_str())) { (patch.durability.as_str()) }
                    }
                }
            }
        }
        @if deploys_new && (action.is_deploy_advanced() || overrides > 0 || !effective.spec.spec.patches.is_empty()) {
            div.preview-result {
                "Result: "
                @if would_be_temporary {
                    strong.preview-result__temporary { "Temporary" }
                } @else {
                    strong.preview-result__standing { "Standing" }
                }
                span.preview-result__detail {
                    " · " (plural(overrides, "override"))
                    " · " (plural(effective.spec.spec.patches.len(), "patch"))
                    @if temporary_patches > 0 {
                        " (" (temporary_patches) " temporary)"
                    }
                }
            }
        }
        div.eyebrow { "Resources" }
        @if config.resource_specs().is_empty() && plan.created.is_empty() {
            div.preview-none { "None" }
        } @else {
            (config.format_resources_planned(namespaced_objs, &plan).await)
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    let suffix = if count == 1 {
        ""
    } else if noun.ends_with("ch") {
        "es"
    } else {
        "s"
    };
    format!("{count} {noun}{suffix}")
}

fn selection_names(config: &DeployConfig) -> Vec<String> {
    let mut names: Vec<String> = config.spec.spec.selections.keys().cloned().collect();
    if !names.iter().any(|n| n == SHA_PARAMETER) {
        names.push(SHA_PARAMETER.to_string());
    }
    names
}

fn override_count(config: &DeployConfig) -> usize {
    selection_names(config)
        .iter()
        .filter(|n| config.selection(n).is_override())
        .count()
}

fn temporary_override_count(config: &DeployConfig) -> usize {
    selection_names(config)
        .iter()
        .filter(|n| config.selection(n).is_temporary())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_know_when_nothing_moves() {
        let a = Shown::plain("1.27.4");
        let unchanged = ParamRow {
            name: "NGINX".into(),
            from: Some(a.clone()),
            to: Target::Value(Some(a.clone())),
            channel: None,
            badge: None,
            compare: None,
            marker: None,
            note: None,
            one_shot: None,
        };
        assert!(unchanged.is_unchanged());
        let changed = ParamRow {
            to: Target::Value(Some(Shown::plain("1.27.5"))),
            ..unchanged
        };
        assert!(!changed.is_unchanged());
        let undeploy = ParamRow {
            to: Target::Value(None),
            ..changed
        };
        assert!(!undeploy.is_unchanged());
        let both_undeployed = ParamRow {
            from: None,
            ..undeploy
        };
        assert!(both_undeployed.is_unchanged());
        let unresolved = ParamRow {
            to: Target::Unresolved("watchtower is down".into()),
            ..both_undeployed
        };
        assert!(unresolved.is_unresolved() && !unresolved.is_unchanged());
    }

    #[test]
    fn compare_links_need_two_different_commits() {
        let a = Shown::commit("o", "r", "aaaaaaa1", None);
        let b = Shown::commit("o", "r", "bbbbbbb2", None);
        assert_eq!(
            compare_url("o", "r", Some(&a), Some(&b)).as_deref(),
            Some("https://github.com/o/r/compare/aaaaaaa1...bbbbbbb2")
        );
        assert!(compare_url("o", "r", Some(&a), Some(&a)).is_none());
        assert!(compare_url("o", "r", None, Some(&a)).is_none());
        let tag = Shown::plain("1.27.4");
        assert!(
            compare_url("o", "r", Some(&tag), Some(&b)).is_none(),
            "only commits compare"
        );
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "override"), "1 override");
        assert_eq!(plural(2, "override"), "2 overrides");
        assert_eq!(plural(0, "patch"), "0 patches");
        assert_eq!(plural(1, "temporary patch"), "1 temporary patch");
    }

    #[test]
    fn shown_shortens_commits_only() {
        let c = Shown::commit("o", "r", "0123456789abcdef", Some(".deploy/x"));
        assert_eq!(c.short, "0123456");
        assert_eq!(
            c.href.as_deref(),
            Some("https://github.com/o/r/tree/0123456789abcdef/.deploy/x")
        );
        let t = Shown::plain("1.27.4-alpine");
        assert_eq!(t.short, "1.27.4-alpine");
        assert!(t.href.is_none());
    }
}
