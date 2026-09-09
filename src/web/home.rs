//! Home: everything that needs a human, and nothing else.
//!
//! Each section renders only when it has rows, so the page is the size of
//! the problem: blocked configs, temporary deployments, unhealthy deploys,
//! configs that have drifted from latest, then the recent activity and the
//! standing overrides as a reminder. When the first three are empty the
//! heading says so in green and a four-fact band gives the shape of the
//! fleet.

use std::collections::{BTreeMap, HashMap};

use futures_util::future::join_all;
use kube::{Client, ResourceExt};
use maud::{html, Markup, DOCTYPE};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::blocker::Blocker;
use crate::db::deploy_config::DeployConfig as DbDeployConfig;
use crate::db::git_branch::GitBranch;
use crate::db::git_repo::GitRepo;
use crate::kubernetes::api::get_all_deploy_configs;
use crate::kubernetes::parameters::{ImageRef, ParameterSource, SHA_PARAMETER};
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::selections::Mode;
use crate::kubernetes::DeployConfig;
use crate::prelude::*;
use crate::watchtower::Watchtower;
use crate::web::deploy_history::{
    render_strips, render_team_select, revisions_for_teams, teams_of,
};
use crate::web::feed;
use crate::web::formatting;
use crate::web::header;
use crate::web::watchdog::{check_deploy_config_health, HealthStatus};

/// How many feed items "Recent activity" shows.
const ACTIVITY_LIMIT: usize = 6;

/// A config whose resources are failing, with the health page's message.
pub struct Unhealthy {
    pub name: String,
    pub message: String,
    pub since: Option<i64>,
}

/// A config a deploy would change: the moving parameters in the preview's
/// syntax, and the autodeploy and durability state that says whether the
/// drift is transient or worth reading.
pub struct Drift {
    pub name: String,
    pub lines: Vec<DriftLine>,
    pub autodeploy: bool,
    pub temporary: bool,
    pub pinned: bool,
}

pub enum DriftLine {
    /// `SHA 9c31be7 → 2f9ab30 (fix/upload-timeout)`
    Moves {
        name: String,
        from: String,
        to: String,
        channel: String,
    },
    /// `NGINX 1.27.4 pinned, 1.27.6 available`
    HeldBack {
        name: String,
        value: String,
        available: String,
    },
    /// `config c1a2b3c → d4e5f6a`
    Config { from: String, to: String },
}

/// A standing override, for the quietest section.
pub struct Standing {
    pub name: String,
    pub what: String,
    pub since: Option<i64>,
}

/// Everything the page shows, gathered once.
pub struct HomeData {
    pub configs: Vec<DeployConfig>,
    pub blockers: Vec<Blocker>,
    pub unhealthy: Vec<Unhealthy>,
    pub healthy_count: usize,
    pub drift: Vec<Drift>,
    pub activity: Vec<feed::Item>,
    pub standing: Vec<Standing>,
    pub last_deploy: Option<i64>,
    pub teams: Vec<String>,
    pub selected_team: Option<String>,
}

impl HomeData {
    pub fn temporary(&self) -> Vec<&DeployConfig> {
        self.configs
            .iter()
            .filter(|c| c.is_temporary_deployment())
            .collect()
    }

    pub fn all_quiet(&self) -> bool {
        self.blockers.is_empty() && self.temporary().is_empty() && self.unhealthy.is_empty()
    }
}

/// A tag channel: the range and the variant followed.
type Channel = (String, Option<String>);

/// The highest tag of each image that matches each channel asked for, from
/// one watchtower lookup per image. Images watchtower cannot answer for
/// are absent.
async fn latest_tags(
    wanted: &BTreeMap<ImageRef, Vec<Channel>>,
) -> HashMap<(ImageRef, Channel), String> {
    let lookups = wanted
        .keys()
        .map(|image| async move { (image.clone(), Watchtower::for_preview().lookup(image).await) });
    let mut out = HashMap::new();
    for (image, result) in join_all(lookups).await {
        let repo = match result {
            Ok(Some(repo)) => repo,
            Ok(None) => continue,
            Err(e) => {
                log::warn!(
                    "watchtower lookup for {}/{} failed: {}",
                    image.registry,
                    image.name,
                    e
                );
                continue;
            }
        };
        let candidates: Vec<crate::kubernetes::tags::Candidate> = repo
            .tag
            .iter()
            .filter(|t| t.active)
            .filter_map(crate::kubernetes::tags::Candidate::of)
            .collect();
        for channel in wanted.get(&image).into_iter().flatten() {
            if let Ok(Some(best)) = crate::kubernetes::tags::highest_matching(
                candidates.iter().copied(),
                &channel.0,
                channel.1.as_deref(),
            ) {
                out.insert((image.clone(), channel.clone()), best);
            }
        }
    }
    out
}

/// The latest successful build on a branch of a repo, from the database.
fn latest_on(
    conn: &PooledConnection<SqliteConnectionManager>,
    owner: &str,
    repo: &str,
    branch: &str,
    successful_only: bool,
) -> Option<String> {
    let repo = GitRepo::get_by_name(owner, repo, conn).ok().flatten()?;
    let branch = GitBranch::get_by_name(branch, repo.id, conn)
        .ok()
        .flatten()?;
    let commit = if successful_only {
        branch.latest_successful_build(conn).ok().flatten()
    } else {
        branch.latest_build(conn).ok().flatten()
    };
    commit.map(|c| c.sha)
}

/// What a deploy of latest would change on each deployed config.
async fn drift_of(
    conn: &PooledConnection<SqliteConnectionManager>,
    configs: &[DeployConfig],
) -> Vec<Drift> {
    // One watchtower round trip per image, whatever the number of configs.
    let mut wanted: BTreeMap<ImageRef, Vec<Channel>> = BTreeMap::new();
    for config in configs {
        for (pname, source) in &config.spec.spec.parameters {
            let (Some(image), Some(default)) = (source.image_ref(), source.default_channel())
            else {
                continue;
            };
            let pattern = match config.selection(pname).mode() {
                Mode::Track(p) => p.to_string(),
                _ => default.to_string(),
            };
            let channel = (pattern, source.variant().map(String::from));
            let channels = wanted.entry(image).or_default();
            if !channels.contains(&channel) {
                channels.push(channel);
            }
        }
    }
    let latest = latest_tags(&wanted).await;

    let mut out = Vec::new();
    for config in configs {
        if config.is_orphaned() {
            continue;
        }
        let state = config.deployment_state();
        if matches!(state, DeploymentState::Undeployed) {
            continue;
        }
        let deployed = config.parameter_values();
        let mut lines = Vec::new();
        let mut pinned = false;
        let mut names: Vec<&String> = config.spec.spec.parameters.keys().collect();
        names.sort_by_key(|n| (n.as_str() != SHA_PARAMETER, n.as_str()));
        for pname in names {
            let Some(source) = config.spec.spec.parameters.get(pname) else {
                continue;
            };
            let selection = config.selection(pname);
            let current = deployed.get(pname).cloned().unwrap_or_default();
            match source {
                ParameterSource::Commit {
                    owner,
                    repo,
                    branch,
                } => {
                    let channel = match selection.mode() {
                        Mode::Pin(_) => {
                            pinned = true;
                            continue;
                        }
                        Mode::Track(b) => b.to_string(),
                        Mode::Default => branch.clone(),
                    };
                    if let Some(sha) = latest_on(conn, owner, repo, &channel, true) {
                        if sha != current {
                            lines.push(DriftLine::Moves {
                                name: pname.clone(),
                                from: formatting::format_short_sha(&current).to_string(),
                                to: formatting::format_short_sha(&sha).to_string(),
                                channel,
                            });
                        }
                    }
                }
                ParameterSource::Tag {
                    image,
                    pattern,
                    variant,
                } => {
                    let image = ImageRef::parse(image);
                    match selection.mode() {
                        Mode::Pin(value) => {
                            pinned = true;
                            if let Some(best) =
                                latest.get(&(image, (pattern.clone(), variant.clone())))
                            {
                                if best != value {
                                    lines.push(DriftLine::HeldBack {
                                        name: pname.clone(),
                                        value: value.to_string(),
                                        available: best.clone(),
                                    });
                                }
                            }
                        }
                        mode => {
                            let range = match mode {
                                Mode::Track(p) => p.to_string(),
                                _ => pattern.clone(),
                            };
                            if let Some(best) =
                                latest.get(&(image, (range.clone(), variant.clone())))
                            {
                                if *best != current {
                                    lines.push(DriftLine::Moves {
                                        name: pname.clone(),
                                        from: current.clone(),
                                        to: best.clone(),
                                        channel: ParameterSource::channel_label(
                                            &range,
                                            variant.as_deref(),
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                ParameterSource::Value { .. } => {
                    if matches!(selection.mode(), Mode::Pin(_)) {
                        pinned = true;
                    }
                }
            }
        }
        // The config commit itself.
        let cfg = config.config_repository();
        let deployed_cfg = config
            .status
            .as_ref()
            .and_then(|s| s.config.as_ref())
            .map(|c| c.sha.clone());
        let branch = config
            .status
            .as_ref()
            .and_then(|s| s.config.as_ref())
            .and_then(|c| c.branch.clone())
            .unwrap_or_else(|| "master".to_string());
        if let (Some(from), Some(to)) = (
            deployed_cfg,
            latest_on(conn, &cfg.owner, &cfg.repo, &branch, false),
        ) {
            if from != to {
                lines.push(DriftLine::Config {
                    from: formatting::format_short_sha(&from).to_string(),
                    to: formatting::format_short_sha(&to).to_string(),
                });
            }
        }
        if !lines.is_empty() {
            out.push(Drift {
                name: config.name_any(),
                lines,
                autodeploy: config.autodeploy(),
                temporary: config.is_temporary_deployment(),
                pinned,
            });
        }
    }
    out
}

/// Standing overrides and patches across the configs, for the reminder.
fn standing_of(configs: &[DeployConfig]) -> Vec<Standing> {
    let mut out = Vec::new();
    for config in configs {
        let name = config.name_any();
        for (pname, selection) in &config.spec.spec.selections {
            if selection.is_override() && !selection.is_temporary() {
                let what = match selection.mode() {
                    Mode::Track(c) => format!("{pname} tracking {c}"),
                    Mode::Pin(v) => {
                        format!("{pname} pinned {}", formatting::format_short_sha(v))
                    }
                    Mode::Default => continue,
                };
                out.push(Standing {
                    name: name.clone(),
                    what,
                    since: selection
                        .since
                        .as_deref()
                        .and_then(formatting::rfc3339_to_ms),
                });
            }
        }
        for patch in &config.spec.spec.patches {
            if !patch.is_temporary() {
                out.push(Standing {
                    name: name.clone(),
                    what: format!("patch: {}", patch.describe_op()),
                    since: patch.since.as_deref().and_then(formatting::rfc3339_to_ms),
                });
            }
        }
    }
    out
}

/// Failing configs, from the health page's check, plus the healthy count.
async fn health_of(client: &Client, configs: &[DeployConfig]) -> (Vec<Unhealthy>, usize) {
    let checks = configs
        .iter()
        .map(|config| async move { (config, check_deploy_config_health(config, client).await) });
    let mut unhealthy = Vec::new();
    let mut healthy = 0;
    for (config, result) in join_all(checks).await {
        match result {
            Ok((HealthStatus::Error, message)) => unhealthy.push(Unhealthy {
                name: config.name_any(),
                message: message.unwrap_or_else(|| "Error".to_string()),
                since: None,
            }),
            Ok((HealthStatus::Healthy, _)) | Ok((HealthStatus::Info, _)) => healthy += 1,
            Ok(_) => {}
            Err(e) => log::warn!("Health check of {} failed: {}", config.name_any(), e),
        }
    }
    (unhealthy, healthy)
}

/// Gather the page. `client` may be absent: the page then shows what the
/// database knows and says the cluster is unreachable.
pub async fn gather(
    conn: &PooledConnection<SqliteConnectionManager>,
    client: Option<&Client>,
    all_configs: Vec<DeployConfig>,
    teams: Vec<String>,
    selected_team: Option<String>,
) -> HomeData {
    let scope_teams: Vec<String> = match &selected_team {
        Some(team) => vec![team.clone()],
        None => teams.clone(),
    };
    let configs: Vec<DeployConfig> = all_configs
        .into_iter()
        .filter(|c| scope_teams.iter().any(|t| t == c.team()))
        .collect();
    let names: Vec<String> = configs.iter().map(|c| c.name_any()).collect();
    let blockers: Vec<Blocker> = Blocker::all_active(conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|b| names.contains(&b.config_name))
        .collect();
    // Stage timings at debug level: RUST_LOG=cicd::web::home=debug.
    let t = std::time::Instant::now();
    let (unhealthy, healthy_count) = match client {
        Some(client) => health_of(client, &configs).await,
        None => (vec![], 0),
    };
    log::debug!(
        "home: health of {} configs in {:?}",
        configs.len(),
        t.elapsed()
    );
    let t = std::time::Instant::now();
    let drift = drift_of(conn, &configs).await;
    log::debug!("home: drift (watchtower + builds) in {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let revisions = revisions_for_teams(conn, &scope_teams).unwrap_or_default();
    log::debug!("home: {} revisions in {:?}", revisions.len(), t.elapsed());
    let last_deploy = revisions
        .iter()
        .find(|r| r.action == "deploy")
        .map(|r| r.created_at);
    let mut activity = feed::group_bursts(feed::entries(revisions));
    activity.truncate(ACTIVITY_LIMIT);
    let standing = standing_of(&configs);
    let all_teams = DbDeployConfig::all_teams(conn).unwrap_or_default();
    HomeData {
        configs,
        blockers,
        unhealthy,
        healthy_count,
        drift,
        activity,
        standing,
        last_deploy,
        teams: all_teams,
        selected_team,
    }
}

fn section(title: Markup, aside: Option<Markup>, muted: bool, body: Markup) -> Markup {
    html! {
        section.nag {
            div.nag__head {
                h2.nag__title.nag__title--muted[muted] { (title) }
                @if let Some(aside) = aside { div.nag__aside { (aside) } }
            }
            (body)
        }
    }
}

fn render_temporary_summary(config: &DeployConfig) -> String {
    let mut parts = Vec::new();
    let mut names: Vec<&String> = config.spec.spec.selections.keys().collect();
    names.sort_by_key(|n| (n.as_str() != SHA_PARAMETER, n.as_str()));
    if !config.spec.spec.selections.contains_key(SHA_PARAMETER) {
        let derived = config.selection(SHA_PARAMETER);
        if derived.is_temporary() {
            if let Mode::Track(b) = derived.mode() {
                parts.push(format!("SHA tracking {b}"));
            }
        }
    }
    for name in names {
        let s = config.selection(name);
        if !s.is_temporary() {
            continue;
        }
        match s.mode() {
            Mode::Track(c) => parts.push(format!("{name} tracking {c}")),
            Mode::Pin(v) => {
                parts.push(format!("{name} pinned {}", formatting::format_short_sha(v)))
            }
            Mode::Default => {}
        }
    }
    let patches = config
        .spec
        .spec
        .patches
        .iter()
        .filter(|p| p.is_temporary())
        .count();
    if patches > 0 {
        parts.push(format!(
            "{patches} patch{}",
            if patches == 1 { "" } else { "es" }
        ));
    }
    parts.join(", ")
}

fn temporary_by(config: &DeployConfig) -> Option<String> {
    config
        .spec
        .spec
        .selections
        .values()
        .filter(|s| s.is_temporary())
        .filter_map(|s| s.by.clone())
        .next()
        .or_else(|| {
            config
                .spec
                .spec
                .patches
                .iter()
                .filter(|p| p.is_temporary())
                .filter_map(|p| p.by.clone())
                .next()
        })
}

/// The whole page.
pub fn render_home(data: &HomeData, strips: Markup, cluster_reachable: bool) -> Markup {
    let temporary = data.temporary();
    let quiet = data.all_quiet();
    let counts = [
        (data.blockers.len(), "blocked"),
        (temporary.len(), "temporary"),
        (data.unhealthy.len(), "unhealthy"),
        (data.drift.len(), "drifted"),
    ];
    let summary: Vec<String> = counts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| format!("{n} {word}"))
        .collect();
    let autodeploy_on = data.configs.iter().filter(|c| c.autodeploy()).count();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { @if quiet { "Nothing needs a human" } @else { (summary.join(" · ")) } }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.home-page {
                (header::render("home"))
                (strips)
                div class="content" {
                    div.page-head {
                        div {
                            @if quiet {
                                h1.page-head__title.page-head__title--ok { "Nothing needs a human" }
                                div.page-head__sub { "No blockers, no temporary deployments, every rollout healthy" }
                            } @else {
                                h1.page-head__title { "Needs a human" }
                                div.page-head__sub { (summary.join(" · ")) }
                            }
                        }
                        (render_team_select("/", &data.teams, data.selected_team.as_deref()))
                    }
                    @if !cluster_reachable {
                        div.preview-strip.preview-strip--neutral { "The cluster is not reachable; only what the database knows is shown." }
                    }
                    @if quiet {
                        div.stat-strip {
                            div.stat-strip__cell { div.eyebrow { "Deploy configs" } div.stat-strip__value { (data.configs.len()) } }
                            div.stat-strip__cell { div.eyebrow { "Healthy" } div.stat-strip__value { span.status-indicator.status-success {} " " (data.healthy_count) " / " (data.configs.len()) } }
                            div.stat-strip__cell { div.eyebrow { "Autodeploy on" } div.stat-strip__value { (autodeploy_on) } }
                            div.stat-strip__cell { div.eyebrow { "Last deploy" } div.stat-strip__value { @match data.last_deploy { Some(t) => (formatting::format_when(t)), None => "none" } } }
                        }
                    }
                    div.nags {
                        @if !data.blockers.is_empty() {
                            (section(html! { i class="fa fa-hand-paper-o" {} "Blocked configs" }, Some(html! { a href="/blockers" { "Blockers" } }), false, html! {
                                @for b in &data.blockers {
                                    div.nag-row.nag-row--fail {
                                        a.nag-row__config href=(format!("/deploy?selected={}", b.config_name)) { (b.config_name) }
                                        span.nag-row__text { (b.reason) }
                                        span.nag-row__meta { (formatting::format_ago_short(b.created_at)) " · " span.mono { (b.created_by) } }
                                        form.nag-row__action action=(format!("/api/blockers/{}/{}/clear", b.config_name, b.id)) method="post" {
                                            input type="hidden" name="return_url" value="/";
                                            button.link-button type="submit" { "Clear" }
                                        }
                                    }
                                }
                            }))
                        }
                        @if !temporary.is_empty() {
                            (section(html! { i class="fa fa-flask" {} "Temporary deployments" }, None, false, html! {
                                @for config in &temporary {
                                    @let name = config.name_any();
                                    div.nag-row.nag-row--warn.nag-row--wide {
                                        a.nag-row__config href=(format!("/deploy?selected={name}")) { (name) }
                                        span.nag-row__text.mono { (render_temporary_summary(config)) }
                                        span.nag-row__meta {
                                            @if let Some(since) = config.temporary_since() { (formatting::format_ago_short(since)) }
                                            @if let Some(by) = temporary_by(config) { " · " span.mono { (by) } }
                                        }
                                        div.nag-row__action { a.secondary-button href=(format!("/deploy?selected={name}&action=end-temporary")) { "End temporary deployment" } }
                                    }
                                }
                            }))
                        }
                        @if !data.unhealthy.is_empty() {
                            (section(html! { "Unhealthy deploys" }, Some(html! { a href="/watchdog" { "Health" } }), false, html! {
                                @for u in &data.unhealthy {
                                    div.nag-row.nag-row--fail {
                                        a.nag-row__config href=(format!("/deploy?selected={}", u.name)) { (u.name) }
                                        span.nag-row__text { span.status-indicator.status-failure title="Failed" {} " " (u.message) }
                                        span.nag-row__meta { @if let Some(since) = u.since { "since " (formatting::format_when(since)) } }
                                        div.nag-row__action { a href=(format!("/deploy?selected={}", u.name)) { "Resources" } }
                                    }
                                }
                            }))
                        }
                        @if !data.drift.is_empty() {
                            (section(html! { "Drifted from latest" }, Some(html! { span.muted { "what a deploy would change" } }), false, html! {
                                @for d in &data.drift {
                                    div.nag-row.nag-row--top {
                                        a.nag-row__config href=(format!("/deploy?selected={}", d.name)) { (d.name) }
                                        div.nag-row__lines {
                                            @for line in &d.lines {
                                                @match line {
                                                    DriftLine::Moves { name, from, to, channel } => div { (name) " " (from) " → " (to) " " span.muted { "(" (channel) ")" } },
                                                    DriftLine::HeldBack { name, value, available } => div { (name) " " (value) " " span.faint { "pinned, " (available) " available" } },
                                                    DriftLine::Config { from, to } => div.faint { "config " (from) " → " (to) },
                                                }
                                            }
                                        }
                                        span.nag-row__meta {
                                            "autodeploy " span.mono { @if d.autodeploy { "on" } @else { "off" } }
                                            @if d.temporary { " · temporary" } @else if d.pinned { " · standing pin" }
                                        }
                                        div.nag-row__action { a href=(format!("/deploy?selected={}", d.name)) { "Deploy" } }
                                    }
                                }
                            }))
                        }
                        (section(html! { "Recent activity" }, Some(html! { a href="/deploy-history" { "Deploy history" } }), false, html! {
                            @if data.activity.is_empty() {
                                div.nag-empty { "None" }
                            } @else {
                                (feed::render_compact(&data.activity))
                            }
                        }))
                        @if !data.standing.is_empty() {
                            (section(html! { "Standing overrides" }, None, true, html! {
                                @for s in &data.standing {
                                    div.nag-row.nag-row--quiet {
                                        a.nag-row__config href=(format!("/deploy?selected={}", s.name)) { (s.name) }
                                        span.nag-row__text.mono { (s.what) }
                                        span.nag-row__meta.faint { @if let Some(since) = s.since { "since " (formatting::format_date(since)) } }
                                    }
                                }
                            }))
                        }
                    }
                }
            }
        }
    }
}

#[get("/")]
pub async fn home(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    client: Option<web::Data<Client>>,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
    // The shared client from app data; building one per request costs a
    // config load and a TLS setup each time.
    let client: Option<Client> = client.map(|c| c.get_ref().clone());
    let t = std::time::Instant::now();
    let all_configs = match &client {
        Some(client) => get_all_deploy_configs(client).await.unwrap_or_else(|e| {
            log::warn!("Failed to list deploy configs for home: {}", e);
            vec![]
        }),
        None => vec![],
    };
    log::debug!(
        "home: {} configs listed in {:?}",
        all_configs.len(),
        t.elapsed()
    );
    let teams = teams_of(&req);
    let selected_team = query.get("team").filter(|t| !t.is_empty()).cloned();
    let t = std::time::Instant::now();
    let strips = render_strips(&conn, &all_configs);
    log::debug!("home: strips in {:?}", t.elapsed());
    let data = gather(&conn, client.as_ref(), all_configs, teams, selected_team).await;
    let t = std::time::Instant::now();
    let body = render_home(&data, strips, client.is_some()).into_string();
    log::debug!("home: rendered {} bytes in {:?}", body.len(), t.elapsed());
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::deploy_config::{DeployConfigSpec, DeployConfigSpecFields};
    use crate::kubernetes::patches::{ManifestPatch, PatchOp, PatchTarget};
    use crate::kubernetes::repo::Repository;
    use crate::kubernetes::selections::{Durability, Selection};

    fn config(name: &str) -> DeployConfig {
        DeployConfig::new(
            name,
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters: Default::default(),
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "o".into(),
                        repo: "c".into(),
                    },
                    specs: vec![],
                },
            },
        )
    }

    #[test]
    fn standing_overrides_are_listed_and_temporary_ones_summarised() {
        let mut dc = config("web");
        dc.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::pin("1.27.0", Durability::Standing),
        );
        dc.spec.spec.selections.insert(
            SHA_PARAMETER.into(),
            Selection::track("fix/x", Durability::Temporary).with_note(None, Some("kevin")),
        );
        dc.spec.spec.patches.push(ManifestPatch {
            target: PatchTarget {
                file: None,
                kind: "Deployment".into(),
                name: "web".into(),
            },
            op: PatchOp::Replace,
            path: "/spec/replicas".into(),
            value: Some(serde_json::json!(2)),
            durability: Durability::Temporary,
            note: None,
            by: None,
            since: None,
        });
        let standing = standing_of(std::slice::from_ref(&dc));
        assert_eq!(standing.len(), 1);
        assert_eq!(standing[0].what, "NGINX pinned 1.27.0");
        assert_eq!(render_temporary_summary(&dc), "SHA tracking fix/x, 1 patch");
        assert_eq!(temporary_by(&dc).as_deref(), Some("kevin"));
    }
}
