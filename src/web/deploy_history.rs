//! Deploy history, read from revisions.
//!
//! Across teams it is a feed grouped by day, with bursts of autodeploys
//! collapsed so a quiet robot never pushes a human deploy off the screen.
//! For one config it is the same feed with revision numbers, a state band
//! above it, the revision on the cluster marked, and a roll-back
//! confirmation that opens beside the feed. Every revision has a detail
//! page; a revision is immutable, so nothing there is editable.

use crate::db::blocker::Blocker;
use crate::db::deploy_config::DeployConfig as DbDeployConfig;
use crate::db::git_repo::GitRepo;
use crate::db::revision::{Revision, RevisionParameter};
use crate::db::revision_diff::Change;
use crate::kubernetes::api::{get_all_deploy_configs, get_deploy_config};
use crate::kubernetes::parameters::SHA_PARAMETER;
use crate::kubernetes::patches::ManifestPatch;
use crate::kubernetes::selections::{Durability, Mode};
use crate::kubernetes::DeployConfig;
use crate::prelude::*;
use crate::web::feed::{self, Entry};
use crate::web::preview::{render_autodeploy_badge, render_durability_badge};
use crate::web::team_prefs::TeamsCookie;
use crate::web::{formatting, header};
use kube::{Client, ResourceExt};
use maud::{html, Markup, DOCTYPE};
use std::collections::HashMap;

/// How many revisions a listing shows at most.
const HISTORY_LIMIT: usize = 200;

/// GitHub repositories a config's revisions refer to, for compare links.
#[derive(Clone, Default)]
pub struct ConfigRepos {
    pub artifact: Option<(String, String)>,
    pub config: Option<(String, String)>,
}

impl ConfigRepos {
    pub fn for_config(conn: &PooledConnection<SqliteConnectionManager>, name: &str) -> Self {
        let repo = |id: u64| {
            GitRepo::get_by_id(&id, conn)
                .ok()
                .flatten()
                .map(|r| (r.owner_name, r.name))
        };
        DbDeployConfig::get_by_name(name, conn)
            .ok()
            .flatten()
            .map(|dc| ConfigRepos {
                artifact: dc.artifact_repo_id.and_then(repo),
                config: repo(dc.config_repo_id),
            })
            .unwrap_or_default()
    }
}

fn compare_link(
    repo: Option<&(String, String)>,
    from: Option<&str>,
    to: Option<&str>,
) -> Option<String> {
    match (repo, from, to) {
        (Some((owner, name)), Some(from), Some(to)) if from != to => Some(format!(
            "https://github.com/{owner}/{name}/compare/{from}...{to}"
        )),
        _ => None,
    }
}

/// Which revisions a listing shows, from the query string and the teams cookie.
pub enum Scope {
    Config(String),
    Team(String),
    Teams(Vec<String>),
}

impl Scope {
    fn from(req: &actix_web::HttpRequest, query: &HashMap<String, String>) -> Self {
        if let Some(name) = query.get("name").filter(|n| !n.is_empty()) {
            Scope::Config(name.clone())
        } else if let Some(team) = query.get("team").filter(|t| !t.is_empty()) {
            Scope::Team(team.clone())
        } else {
            Scope::Teams(teams_of(req))
        }
    }

    fn fragment_url(&self) -> String {
        match self {
            Scope::Config(name) => format!("/deploy-history-fragment?name={name}"),
            Scope::Team(team) => format!("/deploy-history-fragment?team={team}"),
            Scope::Teams(_) => "/deploy-history-fragment".to_string(),
        }
    }

    pub fn revisions(&self, conn: &PooledConnection<SqliteConnectionManager>) -> Vec<Revision> {
        let result = match self {
            Scope::Config(name) => Revision::list_for(conn, name, HISTORY_LIMIT),
            Scope::Team(team) => Revision::list_for_team(conn, team, HISTORY_LIMIT),
            Scope::Teams(teams) => revisions_for_teams(conn, teams),
        };
        match result {
            Ok(revs) => revs,
            Err(e) => {
                log::error!("Failed to load deploy history: {}", e);
                vec![]
            }
        }
    }
}

/// The teams the cookie selects.
pub fn teams_of(req: &actix_web::HttpRequest) -> Vec<String> {
    TeamsCookie::from_request(req)
        .0
        .map(|set| {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            v
        })
        .unwrap_or_default()
}

/// Revisions across several teams, newest first, capped at the listing limit.
pub fn revisions_for_teams(
    conn: &PooledConnection<SqliteConnectionManager>,
    teams: &[String],
) -> AppResult<Vec<Revision>> {
    let mut acc = Vec::new();
    for team in teams {
        acc.append(&mut Revision::list_for_team(conn, team, HISTORY_LIMIT)?);
    }
    acc.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    acc.truncate(HISTORY_LIMIT);
    Ok(acc)
}

/// The team picker: "All my teams" (the cookie) or one team.
pub fn render_team_select(base: &str, teams: &[String], selected: Option<&str>) -> Markup {
    html! {
        select.team-select aria-label="Teams" onchange="window.location.href = this.value" {
            option value=(base) selected[selected.is_none()] { "All my teams" }
            @for team in teams {
                option value=(format!("{base}?team={team}")) selected[selected == Some(team.as_str())] { (team) }
            }
        }
    }
}

fn all_teams(conn: &PooledConnection<SqliteConnectionManager>) -> Vec<String> {
    DbDeployConfig::all_teams(conn).unwrap_or_else(|e| {
        log::warn!("Failed to list teams: {}", e);
        vec![]
    })
}

/// A cluster client and every config, when the cluster is reachable. The
/// history is readable from the database alone; the cluster only adds the
/// state band and the strips.
async fn live_configs() -> Option<(Client, Vec<DeployConfig>)> {
    let client = Client::try_default().await.ok()?;
    let configs = get_all_deploy_configs(&client).await.ok()?;
    Some((client, configs))
}

fn render_head(title: &str) -> Markup {
    html! {
        head {
            meta charset="UTF-8";
            meta name="viewport" content="width=device-width, initial-scale=1.0";
            title { (title) }
            (header::stylesheet_link())
            (header::scripts())
        }
    }
}

fn render_empty(subject: &str) -> Markup {
    html! {
        div.history-empty {
            div.history-empty__title { "No revisions" }
            div.history-empty__body { "Nothing has been deployed for " (subject) ". " a href="/deploy" { "Deploy page" } }
        }
    }
}

/// The feed for the all-configs and one-team scopes.
pub fn render_feed_page(
    conn: &PooledConnection<SqliteConnectionManager>,
    scope: &Scope,
    strips: Markup,
) -> Markup {
    let revisions = scope.revisions(conn);
    let count = revisions.len();
    let teams = all_teams(conn);
    let selected = match scope {
        Scope::Team(team) => Some(team.as_str()),
        _ => None,
    };
    let subject = match scope {
        Scope::Team(team) => format!("team {team}"),
        _ => "these teams".to_string(),
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            (render_head("Deploy history"))
            body.history-page hx-ext="morph" {
                (header::render("history"))
                (strips)
                div class="content" {
                    div.page-head {
                        div {
                            h1.page-head__title { "Deploy history" }
                            div.page-head__sub { "Most recent first · " (count) " revision" @if count != 1 { "s" } }
                        }
                        (render_team_select("/deploy-history", &teams, selected))
                    }
                    @if revisions.is_empty() {
                        (render_empty(&subject))
                    } @else {
                        div #history-feed hx-get=(scope.fragment_url()) hx-trigger="every 5s" hx-swap="morph:innerHTML" {
                            (feed::render_feed(feed::group_bursts(feed::entries(revisions))))
                        }
                    }
                }
            }
        }
    }
}

/// Tracking, overrides, active patches, blockers and autodeploy, in one
/// band above a config's feed.
fn render_state_band(config: &DeployConfig, blockers: &[Blocker]) -> Markup {
    let default_branch = config.artifact_repository().map(|r| r.branch);
    let sha = config.selection(SHA_PARAMETER);
    let mut names: Vec<String> = config.spec.spec.selections.keys().cloned().collect();
    if !names.iter().any(|n| n == SHA_PARAMETER) {
        names.push(SHA_PARAMETER.to_string());
    }
    names.sort_by_key(|n| (n != SHA_PARAMETER, n.clone()));
    let overrides: Vec<(String, String, Durability)> = names
        .iter()
        .filter_map(|n| {
            let s = config.selection(n);
            let shown = match s.mode() {
                Mode::Track(c) => c.to_string(),
                Mode::Pin(v) => formatting::format_short_sha(v).to_string(),
                Mode::Default => return None,
            };
            Some((n.clone(), shown, s.durability))
        })
        .collect();
    html! {
        div.state-band {
            div {
                div.eyebrow { "Tracking" }
                @match (sha.mode(), &default_branch) {
                    (Mode::Track(channel), _) => {
                        div.state-band__value { (channel) " " span.muted { "(override)" } }
                        @if let Some(d) = &default_branch { div.state-band__sub { "default " (d) } }
                    }
                    (Mode::Pin(value), _) => {
                        div.state-band__value { "pinned " (formatting::format_short_sha(value)) }
                        @if let Some(d) = &default_branch { div.state-band__sub { "default " (d) } }
                    }
                    (Mode::Default, Some(d)) => {
                        div.state-band__value { (d) " " span.muted { "(default)" } }
                    }
                    (Mode::Default, None) => {
                        div.state-band__value.muted { "no artifact" }
                    }
                }
            }
            div {
                div.eyebrow { "Overrides" }
                @if overrides.is_empty() {
                    div.state-band__text { "None" }
                } @else {
                    div.state-band__list {
                        @for (name, shown, durability) in &overrides {
                            div { (name) " " (shown) " " (render_durability_badge(*durability)) }
                        }
                    }
                }
            }
            div {
                div.eyebrow { "Active patches" }
                @if config.spec.spec.patches.is_empty() {
                    div.state-band__text { "None" }
                } @else {
                    div.state-band__list {
                        @for p in &config.spec.spec.patches {
                            div.wrap { (p.describe_op()) " " (render_durability_badge(p.durability)) }
                        }
                    }
                }
            }
            div {
                div.eyebrow { "Blockers" }
                @if blockers.is_empty() {
                    div.state-band__text { "None" }
                } @else {
                    div.state-band__list.sans {
                        @for b in blockers {
                            div { (b.reason) " " span.faint { "(" (formatting::format_ago_short(b.created_at)) ")" } }
                        }
                    }
                }
                div.eyebrow.state-band__gap { "Autodeploy" }
                (render_autodeploy_badge(config.autodeploy()))
            }
        }
    }
}

/// One row of a config's feed: revision number, sentence and changes, the
/// roll-back button when this revision can be replayed.
fn render_config_row(entry: &Entry, current: bool, can_roll_back: bool) -> Markup {
    let rev = &entry.rev;
    html! {
        div class=(format!("feed-row feed-row--config {} {}", entry.tone.class_name(), if current { "feed-row--current" } else { "" })) {
            div {
                div.feed-row__id { "#" (rev.id) }
                @if current { div.feed-row__marker { "DEPLOYED NOW" } }
            }
            div.feed-row__body {
                div.feed-row__sentence {
                    (entry.render_sentence(true))
                    span.muted { " · " (formatting::format_when(rev.created_at)) }
                }
                div.feed-row__changes { (entry.render_changes()) }
                @if entry.rollback_to.is_some() {
                    div.feed-row__note { "Blocker added — deploys refused until cleared" }
                }
            }
            div.feed-row__actions {
                @if can_roll_back {
                    button.secondary-button type="button" hx-get=(format!("/fragments/rollback/{}/{}", rev.config_name, rev.id)) hx-target="#rollback-panel" hx-swap="innerHTML" { "Roll back" }
                }
                a href=(format!("/revisions/{}", rev.id)) { "Detail" }
            }
        }
    }
}

/// The rows for one config, bursts and day groups included.
pub fn render_config_feed(revisions: Vec<Revision>) -> Markup {
    let current = revisions
        .first()
        .filter(|r| r.action != "undeploy")
        .map(|r| r.id);
    let entries = feed::entries(revisions);
    let items = feed::group_bursts(entries);
    let groups = feed::group_days(items);
    html! {
        @for (label, date, items) in groups {
            div.feed-day { (label) span.feed-day__date { (date) } }
            @for item in &items {
                @match item {
                    feed::Item::One(entry) => {
                        @let is_current = current == Some(entry.rev.id);
                        (render_config_row(entry, is_current, entry.rev.action == "deploy" && !is_current))
                    }
                    feed::Item::Burst(entries) => (feed::render_burst(entries)),
                }
            }
        }
    }
}

/// One config's page: strips, the state band, the feed and the roll-back
/// panel's slot beside it.
pub fn render_config_page(
    conn: &PooledConnection<SqliteConnectionManager>,
    name: &str,
    live: Option<&DeployConfig>,
    blockers: &[Blocker],
    strips: Markup,
) -> Markup {
    let revisions = Revision::list_for(conn, name, HISTORY_LIMIT).unwrap_or_else(|e| {
        log::error!("Failed to load history for {}: {}", name, e);
        vec![]
    });
    let orphaned = live.is_some_and(DeployConfig::is_orphaned);
    let any_deploy = revisions.iter().any(|r| r.action == "deploy");
    let rollable = revisions.iter().skip(1).any(|r| r.action == "deploy")
        || revisions
            .first()
            .is_some_and(|r| r.action != "deploy" && any_deploy);
    html! {
        (DOCTYPE)
        html lang="en" {
            (render_head(&format!("History of {name}")))
            body.history-page hx-ext="morph" {
                (header::render("history"))
                (strips)
                div class="content" {
                    div.page-head {
                        div {
                            h1.page-head__title { "History of " (name) }
                            div.page-head__sub {
                                @if orphaned { "Orphaned · nothing to deploy · " }
                                a href="/deploy-history" { "All configs" }
                                " · "
                                a href=(format!("/deploy?selected={name}")) { "Deploy page" }
                            }
                        }
                    }
                    @if let Some(config) = live {
                        (render_state_band(config, blockers))
                    }
                    div.history-split {
                        div.history-split__feed {
                            @if revisions.is_empty() {
                                (render_empty(name))
                            } @else {
                                div #history-feed hx-get=(format!("/deploy-history-fragment?name={name}")) hx-trigger="every 5s" hx-swap="morph:innerHTML" {
                                    (render_config_feed(revisions))
                                }
                                @if !rollable && any_deploy {
                                    div.history-footnote { "Only the current revision is a deploy; nothing older to roll back to." }
                                } @else if !any_deploy && live.is_some() {
                                    div.history-footnote { "No deploy revision to roll back to." }
                                }
                            }
                        }
                        div #rollback-panel {}
                    }
                }
            }
        }
    }
}

/// The value a revision recorded for a parameter, in the deploy preview's
/// row style: the value, then the channel.
fn revision_channel(p: &RevisionParameter) -> Option<String> {
    match (&p.branch, p.kind.as_str()) {
        (Some(b), _) if !b.is_empty() => Some(b.clone()),
        (_, "commit") | (_, "tag") => Some("pinned".to_string()),
        _ => None,
    }
}

fn short(p: &RevisionParameter) -> String {
    if p.kind == "commit" {
        formatting::format_short_sha(&p.value).to_string()
    } else {
        p.value.clone()
    }
}

fn patches_of(rev: &Revision) -> Vec<ManifestPatch> {
    rev.patches
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default()
}

/// The roll-back confirmation: what every parameter returns to, how the
/// patch set is replayed, the blocker that will be added, one reason field.
pub fn render_rollback_panel(config: &DeployConfig, revision: &Revision) -> Markup {
    let name = config.name_any();
    let deployed = config.parameter_values();
    let current_config_sha = config
        .status
        .as_ref()
        .and_then(|s| s.config.as_ref())
        .map(|c| c.sha.clone());
    let current_patches = &config.spec.spec.patches;
    let replayed = patches_of(revision);
    let kept: Vec<&ManifestPatch> = replayed
        .iter()
        .filter(|p| current_patches.contains(p))
        .collect();
    let restored: Vec<&ManifestPatch> = replayed
        .iter()
        .filter(|p| !current_patches.contains(p))
        .collect();
    let dropped: Vec<&ManifestPatch> = current_patches
        .iter()
        .filter(|p| !replayed.contains(p))
        .collect();
    let mut params = revision.parameters.clone();
    params.sort_by_key(|p| (p.name != SHA_PARAMETER, p.name.clone()));
    html! {
        div.rollback-panel {
            div.patch-flow__eyebrow { "Confirm roll back" }
            h3.rollback-panel__title { "Roll back " (name) " to " span.mono { "#" (revision.id) } }
            div.rollback-panel__lead { "Every input returns to what it was at #" (revision.id) " — values and patches alike. Selections are untouched." }
            div.param-rows.param-rows--tight {
                div.param-row {
                    span.param-row__name { "CONFIG:" }
                    span.param-row__value {
                        @match (&current_config_sha, &revision.config_sha) {
                            (Some(from), Some(to)) if from == to => { span.param-value { (formatting::format_short_sha(to)) } }
                            (from, Some(to)) => {
                                @match from { Some(f) => span.param-old { (formatting::format_short_sha(f)) }, None => span.param-undeployed { "Undeployed" } }
                                span.param-arrow { "→" }
                                strong.param-new title=(to) { (formatting::format_short_sha(to)) }
                            }
                            (_, None) => { span.param-undeployed { "Undeployed" } }
                        }
                        @if let Some(b) = &revision.config_branch { span.param-channel { "(" (b) ")" } }
                    }
                }
                @for p in &params {
                    @let from = deployed.get(&p.name).cloned();
                    @let same = from.as_deref() == Some(p.value.as_str());
                    div.param-row.param-row--same[same] {
                        span.param-row__name { (p.name) ":" }
                        span.param-row__value {
                            @if same {
                                span.param-value { (short(p)) }
                            } @else {
                                @match &from {
                                    Some(f) => span.param-old { (if p.kind == "commit" { formatting::format_short_sha(f).to_string() } else { f.clone() }) },
                                    None => span.param-undeployed { "Undeployed" },
                                }
                                span.param-arrow { "→" }
                                strong.param-new title=(p.value) { (short(p)) }
                            }
                            @if let Some(channel) = revision_channel(p) { span.param-channel { "(" (channel) ")" } }
                        }
                    }
                }
            }
            div.rollback-panel__patches-title { "Patches: " span.mono.faint { "replayed as they were at #" (revision.id) } }
            div.rollback-panel__patches {
                @if replayed.is_empty() && dropped.is_empty() { div.faint { "none" } }
                @for p in &kept { div.wrap { "kept: " (p.describe_op()) } }
                @for p in &restored { div.wrap { "restored: " (p.describe_op()) } }
                @for p in &dropped { div.wrap.faint { "dropped: " (p.describe_op()) " " span.muted { "(added after #" (revision.id) ")" } } }
            }
            div.alert.alert-danger {
                div.alert-header { i class="fa fa-hand-paper-o" {} " A blocker will be added" }
                div.alert-content { div.details { "Autodeploy and further deploys are refused until it is cleared on the Blockers page." } }
            }
            form action=(format!("/api/rollback/{}/{}", name, revision.id)) method="post" {
                input type="hidden" name="return_url" value=(format!("/deploy-history/{name}"));
                label.patch-flow__label for="rollback-reason" { "Reason" }
                input #rollback-reason type="text" name="reason" placeholder="why this revision" autocomplete="off";
                button.primary-button.danger.block type="submit" { "Roll back and hold" }
            }
            div.rollback-panel__cancel {
                a href=(format!("/deploy-history/{name}")) hx-get=(format!("/fragments/rollback/{name}/closed")) hx-target="#rollback-panel" hx-swap="innerHTML" { "Cancel" }
            }
        }
    }
}

/// A revision's page: facts, parameters with compare links, patches, the
/// changes against the previous revision, and where to go next.
pub fn render_revision_page(
    conn: &PooledConnection<SqliteConnectionManager>,
    revision: &Revision,
    previous: Option<&Revision>,
    strips: Markup,
) -> Markup {
    let entry = Entry::from(revision.clone(), previous);
    let name = &revision.config_name;
    let repos = ConfigRepos::for_config(conn, name);
    let prev_param = |pname: &str| previous.and_then(|p| p.parameter(pname));
    let config_compare = compare_link(
        repos.config.as_ref(),
        previous.and_then(|p| p.config_sha.as_deref()),
        revision.config_sha.as_deref(),
    );
    let changes: Vec<&Change> = entry.changes.iter().collect();
    let mut params = revision.parameters.clone();
    params.sort_by_key(|p| (p.name != SHA_PARAMETER, p.name.clone()));
    let patches = patches_of(revision);
    html! {
        (DOCTYPE)
        html lang="en" {
            (render_head(&format!("Revision #{} of {}", revision.id, name)))
            body.history-page.revision-page {
                (header::render("history"))
                (strips)
                div class="content" {
                    div.revision {
                        div.revision__head {
                            h1 { "Revision " strong.mono { "#" (revision.id) } " of " strong { (name) } }
                            a href=(format!("/deploy-history/{name}")) { "Close" }
                        }
                        @if let Some(to) = entry.rollback_to {
                            div.alert.alert-danger {
                                div.alert-header { i class="fa fa-hand-paper-o" {} " Rolled back to " a href=(format!("/revisions/{to}")) { "#" (to) } " — blocker added" }
                                div.alert-content { div.details {
                                    @if let Some(reason) = &entry.reason { "\u{201c}" (reason) "\u{201d} · " }
                                    a href="/blockers" { (name) " on Blockers" }
                                } }
                            }
                        }
                        div.fact-grid {
                            div { div.eyebrow { "Action" } div.fact { (entry.verb) } }
                            div { div.eyebrow { "Actor" } div.fact.mono { (revision.actor) } }
                            div { div.eyebrow { "Time" } div.fact.mono { (formatting::format_datetime(revision.created_at)) } }
                            @if let Some(to) = entry.rollback_to {
                                div { div.eyebrow { "Replayed" } div.fact.mono { a href=(format!("/revisions/{to}")) { "#" (to) } } }
                            } @else if revision.action != "undeploy" {
                                div { div.eyebrow { "Durability" } (render_durability_badge(if revision.temporary { Durability::Temporary } else { Durability::Standing })) }
                            }
                        }
                        @if entry.rollback_to.is_none() {
                            @if let Some(reason) = &entry.reason {
                                div.revision__reason { "Reason: " span { "\u{201c}" (reason) "\u{201d}" } }
                            }
                        }
                        @if revision.action != "undeploy" {
                            div.eyebrow { "Parameters" }
                            div.param-rows.param-rows--detail {
                                div.param-row {
                                    span.param-row__name { "CONFIG:" }
                                    span.param-row__value {
                                        @match &revision.config_sha {
                                            Some(sha) => { strong.param-new title=(sha) { (formatting::format_short_sha(sha)) } }
                                            None => { span.param-undeployed { "Undeployed" } }
                                        }
                                        @if let Some(b) = &revision.config_branch { span.param-channel { "(" (b) ")" } }
                                    }
                                    span.param-row__link { @if let Some(url) = &config_compare { a href=(url) target="_blank" { "compare" } } }
                                }
                                @for p in &params {
                                    @let compare = if p.kind == "commit" { compare_link(repos.artifact.as_ref(), prev_param(&p.name).map(|x| x.value.as_str()), Some(&p.value)) } else { None };
                                    div.param-row.param-row--same[p.kind == "value"] {
                                        span.param-row__name { (p.name) ":" }
                                        span.param-row__value {
                                            @if p.kind == "value" {
                                                span.param-value { (p.value) }
                                            } @else {
                                                strong.param-new title=(p.value) { (short(p)) }
                                            }
                                            @if let Some(channel) = revision_channel(p) { span.param-channel { "(" (channel) ")" } }
                                        }
                                        span.param-row__link { @if let Some(url) = compare { a href=(url) target="_blank" { "compare" } } }
                                    }
                                }
                            }
                            div.eyebrow { "Patches active" }
                            @if patches.is_empty() {
                                div.preview-none { "None" }
                            } @else {
                                div.revision__patches {
                                    @for p in &patches {
                                        div.revision__patch { code { (p.describe_op()) } " " (render_durability_badge(p.durability)) }
                                    }
                                }
                            }
                        }
                        div.eyebrow {
                            @match previous { Some(prev) => { "Changes from #" (prev.id) } None => { "Changes" } }
                        }
                        div.revision__changes {
                            @if changes.is_empty() { div.faint { "no change" } }
                            @for c in &changes {
                                div.wrap.faint[matches!(c, Change::Config { .. })] { (c.describe()) }
                            }
                        }
                        div.revision__links {
                            a href=(format!("/deploy-history/{name}")) { "History of " (name) }
                            a href=(format!("/deploy?selected={name}")) { "Deploy page" }
                            @if let Some(url) = &config_compare { a href=(url) target="_blank" { "Config compare" } }
                        }
                    }
                }
            }
        }
    }
}

fn db_error(e: impl std::fmt::Display) -> HttpResponse {
    log::error!("Failed to get database connection: {}", e);
    HttpResponse::InternalServerError()
        .content_type("text/html; charset=utf-8")
        .body("Failed to connect to database")
}

fn page(markup: Markup) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

/// The temporary and held strips, from what the cluster and the database
/// say right now.
pub fn render_strips(
    conn: &PooledConnection<SqliteConnectionManager>,
    configs: &[DeployConfig],
) -> Markup {
    let held = Blocker::all_active(conn).unwrap_or_default();
    html! {
        (crate::web::blockers::render_held_strip(&held))
        (crate::web::selections::render_temporary_strip(configs))
    }
}

#[get("/deploy-history/{name}")]
pub async fn deploy_history(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<String>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return db_error(e),
    };
    let name = path.into_inner();
    let live = live_configs().await;
    let teams = TeamsCookie::from_request(&req);
    let (config, strips) = match &live {
        Some((_, configs)) => (
            configs.iter().find(|c| c.name_any() == name).cloned(),
            render_strips(&conn, &teams.filter_configs(configs)),
        ),
        None => (None, html! {}),
    };
    let blockers = Blocker::active_for(&conn, &name).unwrap_or_default();
    page(render_config_page(
        &conn,
        &name,
        config.as_ref(),
        &blockers,
        strips,
    ))
}

#[get("/deploy-history")]
pub async fn deploy_history_index(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return db_error(e),
    };
    let scope = Scope::from(&req, &query);
    if let Scope::Config(name) = &scope {
        return HttpResponse::SeeOther()
            .append_header(("Location", format!("/deploy-history/{name}")))
            .finish();
    }
    let strips = match live_configs().await {
        Some((_, configs)) => render_strips(
            &conn,
            &TeamsCookie::from_request(&req).filter_configs(&configs),
        ),
        None => html! {},
    };
    page(render_feed_page(&conn, &scope, strips))
}

#[get("/deploy-history-fragment")]
pub async fn deploy_history_fragment(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return db_error(e),
    };
    let scope = Scope::from(&req, &query);
    let revisions = scope.revisions(&conn);
    let markup = match scope {
        Scope::Config(_) => render_config_feed(revisions),
        _ => feed::render_feed(feed::group_bursts(feed::entries(revisions))),
    };
    page(markup)
}

#[get("/revisions/{id}")]
pub async fn revision_detail(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<i64>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return db_error(e),
    };
    let id = path.into_inner();
    let revision = match Revision::get(&conn, id) {
        Ok(Some(r)) => r,
        Ok(None) => return HttpResponse::NotFound().body(format!("No revision #{id}")),
        Err(e) => {
            log::error!("Failed to load revision {}: {}", id, e);
            return HttpResponse::InternalServerError().body("Failed to load revision");
        }
    };
    let previous = revision.previous(&conn).unwrap_or_else(|e| {
        log::warn!("Failed to load the revision before {}: {}", id, e);
        None
    });
    let strips = match live_configs().await {
        Some((_, configs)) => render_strips(
            &conn,
            &TeamsCookie::from_request(&req).filter_configs(&configs),
        ),
        None => html! {},
    };
    page(render_revision_page(
        &conn,
        &revision,
        previous.as_ref(),
        strips,
    ))
}

#[get("/fragments/rollback/{name}/{revision}")]
pub async fn rollback_panel(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<(String, String)>,
) -> impl Responder {
    let (name, revision) = path.into_inner();
    // The Cancel link: empty the panel.
    if revision == "closed" {
        return page(html! {});
    }
    let Ok(revision) = revision.parse::<i64>() else {
        return HttpResponse::BadRequest().body("revision must be a number");
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return db_error(e),
    };
    let revision = match Revision::get(&conn, revision) {
        Ok(Some(r)) if r.config_name == name => r,
        Ok(_) => return HttpResponse::NotFound().body("No such revision on this config"),
        Err(e) => {
            log::error!("Failed to load revision {}: {}", revision, e);
            return HttpResponse::InternalServerError().body("Failed to load revision");
        }
    };
    let config = match Client::try_default().await {
        Ok(client) => get_deploy_config(&client, &name).await.ok().flatten(),
        Err(_) => None,
    };
    let Some(config) = config else {
        return page(html! {
            div.rollback-panel { p.muted { "The cluster is not reachable; the current state cannot be compared." } }
        });
    };
    page(render_rollback_panel(&config, &revision))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_link_needs_repo_and_two_different_shas() {
        let repo = ("o".to_string(), "r".to_string());
        assert_eq!(
            compare_link(Some(&repo), Some("a"), Some("b")).as_deref(),
            Some("https://github.com/o/r/compare/a...b")
        );
        assert!(compare_link(Some(&repo), Some("a"), Some("a")).is_none());
        assert!(compare_link(Some(&repo), None, Some("b")).is_none());
        assert!(compare_link(None, Some("a"), Some("b")).is_none());
    }

    #[test]
    fn revision_channels_read_like_the_preview() {
        let p = |kind: &str, branch: Option<&str>| RevisionParameter {
            name: "X".into(),
            kind: kind.into(),
            value: "0123456789".into(),
            branch: branch.map(String::from),
        };
        assert_eq!(
            revision_channel(&p("commit", Some("master"))).as_deref(),
            Some("master")
        );
        assert_eq!(
            revision_channel(&p("commit", None)).as_deref(),
            Some("pinned")
        );
        assert_eq!(
            revision_channel(&p("tag", Some("1.27.*"))).as_deref(),
            Some("1.27.*")
        );
        assert_eq!(revision_channel(&p("value", None)), None);
        assert_eq!(short(&p("commit", None)), "0123456");
        assert_eq!(short(&p("tag", None)), "0123456789");
    }

    #[test]
    fn team_select_links_each_team() {
        let html = render_team_select(
            "/deploy-history",
            &["infra".into(), "media".into()],
            Some("media"),
        )
        .into_string();
        assert!(html.contains("value=\"/deploy-history\""));
        assert!(html.contains("value=\"/deploy-history?team=media\" selected"));
        assert!(!html.contains("value=\"/deploy-history?team=infra\" selected"));
    }
}
