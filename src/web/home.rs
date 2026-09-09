//! Home: everything that needs a human, and nothing else.
//!
//! Each section renders only when it has rows, so the page is the size of
//! the problem: blocked configs, temporary deployments, unhealthy deploys,
//! configs that have drifted from latest, configs whose tag pattern is
//! behind what the image publishes, then the recent activity and the
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

/// A config whose declared pattern excludes the newest tag its image
/// publishes: an upgrade that only a config change can take, so it is
/// told apart from drift, which a deploy of latest clears.
pub struct Upgrade {
    pub name: String,
    pub lines: Vec<UpgradeLine>,
    /// The declaration on GitHub, where the pattern lives.
    pub config_url: String,
}

/// `NGINX 1.27.6 → 1.29.1 (outside 1.27.*)`
pub struct UpgradeLine {
    pub name: String,
    pub current: String,
    pub newest: String,
    pub channel: String,
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
    pub upgrades: Vec<Upgrade>,
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

/// The best tag of each image per channel, from [`latest_tags`].
type Latest = HashMap<(ImageRef, Channel), String>;

/// The range that admits every version: its best is the newest tag the
/// image publishes, or the newest of the variant followed.
const ANY: &str = "*";

/// Every channel a tag parameter follows, plus [`ANY`] beside each, so one
/// watchtower round trip per image answers both drift and upgrades.
fn wanted_channels(configs: &[DeployConfig]) -> BTreeMap<ImageRef, Vec<Channel>> {
    let mut wanted: BTreeMap<ImageRef, Vec<Channel>> = BTreeMap::new();
    for config in configs {
        for (pname, source) in &config.spec.spec.parameters {
            let (Some(image), Some(default)) = (source.image_ref(), source.default_channel())
            else {
                continue;
            };
            let variant = source.variant().map(String::from);
            let mut channels = vec![
                (default.to_string(), variant.clone()),
                (ANY.to_string(), variant.clone()),
            ];
            if let Mode::Track(p) = config.selection(pname).mode() {
                channels.push((p.to_string(), variant));
            }
            let entry = wanted.entry(image).or_default();
            for channel in channels {
                if !entry.contains(&channel) {
                    entry.push(channel);
                }
            }
        }
    }
    wanted
}

/// The highest tag of each image that matches each channel asked for, from
/// one watchtower lookup per image. Images watchtower cannot answer for
/// are absent.
async fn latest_tags(wanted: &BTreeMap<ImageRef, Vec<Channel>>) -> Latest {
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

/// The branch the deployed config came from; `master` until known.
fn config_branch(config: &DeployConfig) -> String {
    config
        .status
        .as_ref()
        .and_then(|s| s.config.as_ref())
        .and_then(|c| c.branch.clone())
        .unwrap_or_else(|| "master".to_string())
}

/// The declaration on GitHub, at the branch the deployed config came from.
fn config_file_url(config: &DeployConfig) -> String {
    let repo = config.config_repository();
    format!(
        "https://github.com/{}/{}/blob/{}/.deploy/{}.yaml",
        repo.owner,
        repo.repo,
        config_branch(config),
        config.name_any()
    )
}

/// Whether a config is one the home page speaks for: on the cluster and
/// deployed.
fn is_live(config: &DeployConfig) -> bool {
    !config.is_orphaned() && !matches!(config.deployment_state(), DeploymentState::Undeployed)
}

/// What a deploy of latest would change on each deployed config.
fn drift_of(
    conn: &PooledConnection<SqliteConnectionManager>,
    configs: &[DeployConfig],
    latest: &Latest,
) -> Vec<Drift> {
    let mut out = Vec::new();
    for config in configs {
        if !is_live(config) {
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
        if let (Some(from), Some(to)) = (
            deployed_cfg,
            latest_on(conn, &cfg.owner, &cfg.repo, &config_branch(config), false),
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

/// Deployed configs whose declared pattern excludes the newest tag the
/// image publishes. The test is the pattern itself: the newest tag of the
/// image (of the variant followed, if one is) is not the best the pattern
/// admits, so following it means editing the declaration, whatever the
/// size of the jump. Overrides are ignored: the declaration is what the
/// change edits.
fn upgrades_of(configs: &[DeployConfig], latest: &Latest) -> Vec<Upgrade> {
    let mut out = Vec::new();
    for config in configs {
        if !is_live(config) {
            continue;
        }
        let deployed = config.parameter_values();
        let mut lines = Vec::new();
        for (pname, source) in &config.spec.spec.parameters {
            let ParameterSource::Tag {
                image,
                pattern,
                variant,
            } = source
            else {
                continue;
            };
            let image = ImageRef::parse(image);
            let Some(newest) = latest.get(&(image.clone(), (ANY.to_string(), variant.clone())))
            else {
                continue;
            };
            let in_range = latest.get(&(image, (pattern.clone(), variant.clone())));
            if in_range == Some(newest) {
                continue;
            }
            lines.push(UpgradeLine {
                name: pname.clone(),
                current: deployed.get(pname).cloned().unwrap_or_default(),
                newest: newest.clone(),
                channel: ParameterSource::channel_label(pattern, variant.as_deref()),
            });
        }
        if !lines.is_empty() {
            out.push(Upgrade {
                name: config.name_any(),
                lines,
                config_url: config_file_url(config),
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
    let latest = latest_tags(&wanted_channels(&configs)).await;
    log::debug!("home: watchtower in {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let drift = drift_of(conn, &configs, &latest);
    let upgrades = upgrades_of(&configs, &latest);
    log::debug!("home: drift and upgrades in {:?}", t.elapsed());
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
        upgrades,
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
                        @if !data.upgrades.is_empty() {
                            (section(html! { "Newer than the pattern" }, Some(html! { span.muted { "needs a config change" } }), false, html! {
                                @for u in &data.upgrades {
                                    div.nag-row.nag-row--top {
                                        a.nag-row__config href=(format!("/deploy?selected={}", u.name)) { (u.name) }
                                        div.nag-row__lines {
                                            @for line in &u.lines {
                                                div { (line.name) " " (line.current) " → " (line.newest) " " span.muted { "(outside " (line.channel) ")" } }
                                            }
                                        }
                                        span.nag-row__meta {}
                                        div.nag-row__action { a href=(u.config_url) { "Edit config" } }
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
    use crate::kubernetes::deploy_config::{
        DeployConfigSpec, DeployConfigSpecFields, DeployConfigStatus,
    };
    use crate::kubernetes::parameters::ParameterValue;
    use crate::kubernetes::patches::{ManifestPatch, PatchOp, PatchTarget};
    use crate::kubernetes::repo::{Repository, ShaMaybeBranch};
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

    fn tag(image: &str, pattern: &str, variant: Option<&str>) -> ParameterSource {
        ParameterSource::Tag {
            image: image.into(),
            pattern: pattern.into(),
            variant: variant.map(String::from),
        }
    }

    fn deployed(mut dc: DeployConfig, values: &[(&str, &str)]) -> DeployConfig {
        let mut status = DeployConfigStatus {
            config: Some(ShaMaybeBranch {
                sha: "abc".into(),
                branch: Some("main".into()),
            }),
            ..Default::default()
        };
        for (name, value) in values {
            status.parameters.insert(
                (*name).into(),
                ParameterValue::Tag {
                    value: (*value).into(),
                    pattern: None,
                    digest: None,
                },
            );
        }
        dc.status = Some(status);
        dc
    }

    fn best(
        image: &str,
        pattern: &str,
        variant: Option<&str>,
        tag: &str,
    ) -> ((ImageRef, Channel), String) {
        (
            (
                ImageRef::parse(image),
                (pattern.into(), variant.map(String::from)),
            ),
            tag.into(),
        )
    }

    #[test]
    fn every_channel_is_asked_for_beside_the_one_that_admits_everything() {
        let mut dc = config("web");
        dc.spec
            .spec
            .parameters
            .insert("NGINX".into(), tag("nginx", "1.27.*", None));
        dc.spec.spec.parameters.insert(
            "MQTT".into(),
            tag("eclipse-mosquitto", "^2.1", Some("alpine")),
        );
        dc.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::track_pattern("^1.28", Durability::Standing),
        );
        let wanted = wanted_channels(std::slice::from_ref(&dc));
        assert_eq!(
            wanted[&ImageRef::parse("nginx")],
            vec![
                ("1.27.*".to_string(), None),
                ("*".to_string(), None),
                ("^1.28".to_string(), None)
            ]
        );
        assert_eq!(
            wanted[&ImageRef::parse("eclipse-mosquitto")],
            vec![
                ("^2.1".to_string(), Some("alpine".into())),
                ("*".to_string(), Some("alpine".into()))
            ]
        );
    }

    #[test]
    fn upgrades_are_the_tags_the_declared_pattern_excludes() {
        let mut dc = config("web");
        dc.spec
            .spec
            .parameters
            .insert("NGINX".into(), tag("nginx", "1.27.*", None));
        dc.spec.spec.parameters.insert(
            "MQTT".into(),
            tag("eclipse-mosquitto", "^2.1", Some("alpine")),
        );
        dc.spec
            .spec
            .parameters
            .insert("PG".into(), tag("postgres", "^15.11", None));
        let dc = deployed(
            dc,
            &[
                ("NGINX", "1.27.4"),
                ("MQTT", "2.1.2-alpine"),
                ("PG", "15.19"),
            ],
        );
        let latest: Latest = [
            // Newest is outside the range: an upgrade, whatever the jump.
            best("nginx", "1.27.*", None, "1.27.6"),
            best("nginx", "*", None, "1.29.1"),
            // The variant's newest is what the range admits: nothing to say,
            // even though the bare image has moved on.
            best("eclipse-mosquitto", "^2.1", Some("alpine"), "2.1.3-alpine"),
            best("eclipse-mosquitto", "*", Some("alpine"), "2.1.3-alpine"),
            // A two-part version: the range admits nothing at all.
            best("postgres", "*", None, "18.1"),
        ]
        .into_iter()
        .collect();
        let upgrades = upgrades_of(std::slice::from_ref(&dc), &latest);
        assert_eq!(upgrades.len(), 1);
        let up = &upgrades[0];
        assert_eq!(up.name, "web");
        assert_eq!(
            up.config_url,
            "https://github.com/o/c/blob/main/.deploy/web.yaml"
        );
        let lines: Vec<(&str, &str, &str, &str)> = up
            .lines
            .iter()
            .map(|l| {
                (
                    l.name.as_str(),
                    l.current.as_str(),
                    l.newest.as_str(),
                    l.channel.as_str(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                ("NGINX", "1.27.4", "1.29.1", "1.27.*"),
                ("PG", "15.19", "18.1", "^15.11"),
            ]
        );
        // An undeployed config has nothing to upgrade yet.
        let mut fresh = config("fresh");
        fresh
            .spec
            .spec
            .parameters
            .insert("NGINX".into(), tag("nginx", "1.27.*", None));
        assert!(upgrades_of(std::slice::from_ref(&fresh), &latest).is_empty());
    }
}
