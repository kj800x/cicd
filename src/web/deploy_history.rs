//! Deploy history, read from revisions.
//!
//! Each row is one revision. The artifact and config cells show what was
//! deployed and, when the previous revision of the same config is in the
//! list, a compare link and a "[changed]" marker for the config manifests.

use crate::db::deploy_config::DeployConfig as DbDeployConfig;
use crate::db::git_repo::GitRepo;
use crate::db::revision::Revision;
use crate::kubernetes::parameters::SHA_PARAMETER;
use crate::prelude::*;
use crate::web::team_prefs::TeamsCookie;
use crate::web::{formatting, header};
use chrono::TimeZone;
use chrono_tz::America::New_York;
use maud::{html, Markup, DOCTYPE};
use std::collections::HashMap;

/// How many revisions a listing shows at most.
const HISTORY_LIMIT: usize = 200;

fn format_et_time(timestamp_ms: i64) -> String {
    let utc = match Utc.timestamp_millis_opt(timestamp_ms).single() {
        Some(t) => t,
        None => return "Invalid time".to_string(),
    };
    let local = utc.with_timezone(&New_York);
    local.format("%b %-e, %Y %-I:%M %p").to_string()
}

/// GitHub repositories a config's revisions refer to, for compare links.
#[derive(Clone, Default)]
struct ConfigRepos {
    artifact: Option<(String, String)>,
    config: Option<(String, String)>,
}

/// Resolve `owner/name` pairs for each config once per render.
struct RepoLookup<'a> {
    conn: &'a PooledConnection<SqliteConnectionManager>,
    cache: HashMap<String, ConfigRepos>,
}

impl<'a> RepoLookup<'a> {
    fn new(conn: &'a PooledConnection<SqliteConnectionManager>) -> Self {
        Self {
            conn,
            cache: HashMap::new(),
        }
    }

    fn for_config(&mut self, name: &str) -> ConfigRepos {
        if let Some(found) = self.cache.get(name) {
            return found.clone();
        }
        let repos = DbDeployConfig::get_by_name(name, self.conn)
            .ok()
            .flatten()
            .map(|dc| ConfigRepos {
                artifact: dc.artifact_repo_id.and_then(|id| self.repo(id)),
                config: self.repo(dc.config_repo_id),
            })
            .unwrap_or_default();
        self.cache.insert(name.to_string(), repos.clone());
        repos
    }

    fn repo(&self, id: u64) -> Option<(String, String)> {
        GitRepo::get_by_id(&id, self.conn)
            .ok()
            .flatten()
            .map(|r| (r.owner_name, r.name))
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

fn render_sha_maybe_branch(branch: Option<&str>, sha: Option<&str>) -> Markup {
    match sha {
        Some(sha) => {
            let short = formatting::format_short_sha(sha);
            match branch {
                Some(b) if !b.is_empty() => html! { (b) ":" (short) },
                _ => html! { (short) },
            }
        }
        None => html! { "-" },
    }
}

fn render_artifact_cell(rev: &Revision, prev: Option<&Revision>, repos: &ConfigRepos) -> Markup {
    let param = rev.parameter(SHA_PARAMETER);
    let prev_value = prev
        .and_then(|p| p.parameter(SHA_PARAMETER))
        .map(|p| p.value.as_str());
    let compare = compare_link(
        repos.artifact.as_ref(),
        prev_value,
        param.map(|p| p.value.as_str()),
    );
    html! {
        td class="sha-cell" {
            (render_sha_maybe_branch(param.and_then(|p| p.branch.as_deref()), param.map(|p| p.value.as_str())))
            @if let Some(url) = compare {
                " "
                a class="link-button" href=(url) target="_blank" { "[compare]" }
            }
        }
    }
}

fn render_config_cell(rev: &Revision, prev: Option<&Revision>, repos: &ConfigRepos) -> Markup {
    let changed = match (
        &rev.config_version_hash,
        prev.and_then(|p| p.config_version_hash.as_ref()),
    ) {
        (Some(cur), Some(prev)) => cur != prev,
        _ => false,
    };
    let compare = compare_link(
        repos.config.as_ref(),
        prev.and_then(|p| p.config_sha.as_deref()),
        rev.config_sha.as_deref(),
    );
    html! {
        td class="sha-cell" {
            (render_sha_maybe_branch(rev.config_branch.as_deref(), rev.config_sha.as_deref()))
            @if changed {
                " "
                span { "[changed]" }
            }
            @if let Some(url) = compare {
                " "
                a class="link-button" href=(url) target="_blank" { "[compare]" }
            }
        }
    }
}

fn render_revision_row(rev: &Revision, prev: Option<&Revision>, repos: &ConfigRepos) -> Markup {
    html! {
        tr {
            td class="config-name" {
                a href=(format!("/deploy-history?name={}", rev.config_name)) { (rev.config_name) }
            }
            td class="initiator-cell" {
                @if rev.action == "undeploy" { "undeploy" } @else { "deploy" }
                " · " (rev.actor)
                @if let Some(reason) = &rev.reason { br; span { (reason) } }
            }
            (render_artifact_cell(rev, prev, repos))
            (render_config_cell(rev, prev, repos))
            td class="time-cell" { (format_et_time(rev.created_at)) }
            td class="actions-cell" { (render_row_actions(rev)) }
        }
    }
}

/// Roll back to a deploy revision. Undeploy revisions offer nothing: going
/// back to "nothing deployed" is the undeploy button on the deploy page.
fn render_row_actions(rev: &Revision) -> Markup {
    if rev.action != "deploy" {
        return html! {};
    }
    html! {
        form action=(format!("/api/rollback/{}/{}", rev.config_name, rev.id)) method="post" class="rollback-form" {
            input type="hidden" name="return_url" value=(format!("/deploy-history?name={}", rev.config_name));
            button type="submit" class="link-button" title="Redeploy exactly these values, then hold the config with a blocker" {
                "Roll back to this"
            }
        }
    }
}

fn render_rows(
    conn: &PooledConnection<SqliteConnectionManager>,
    revisions: Vec<Revision>,
) -> Markup {
    let mut lookup = RepoLookup::new(conn);
    let paired = Revision::with_previous(revisions);
    html! {
        @for (rev, prev) in &paired {
            @let repos = lookup.for_config(&rev.config_name);
            (render_revision_row(rev, prev.as_ref(), &repos))
        }
    }
}

fn render_table(
    conn: &PooledConnection<SqliteConnectionManager>,
    revisions: Vec<Revision>,
    fragment_url: &str,
) -> Markup {
    html! {
        table class="history-table" {
            thead {
                tr {
                    th { "Deploy config" }
                    th { "Action" }
                    th { "Artifact" }
                    th { "Config" }
                    th { "Time" }
                    th { "" }
                }
            }
            tbody id="history-tbody"
                hx-get=(fragment_url)
                hx-trigger="load, every 5s"
                hx-swap="morph:innerHTML" {
                (render_rows(conn, revisions))
            }
        }
    }
}

/// Which revisions a listing shows, from the query string and the teams cookie.
enum Scope {
    Config(String),
    Team(String),
    Teams(Vec<String>),
}

impl Scope {
    fn from(req: &actix_web::HttpRequest, query: &HashMap<String, String>) -> Self {
        if let Some(name) = query.get("name") {
            Scope::Config(name.clone())
        } else if let Some(team) = query.get("team") {
            Scope::Team(team.clone())
        } else {
            let teams = TeamsCookie::from_request(req)
                .0
                .map(|set| set.into_iter().collect())
                .unwrap_or_default();
            Scope::Teams(teams)
        }
    }

    fn title(&self) -> String {
        match self {
            Scope::Config(name) => format!("Deploy history for {name}"),
            Scope::Team(team) => format!("Deploy history for team {team}"),
            Scope::Teams(_) => "Deploy history".to_string(),
        }
    }

    fn fragment_url(&self) -> String {
        match self {
            Scope::Config(name) => format!("/deploy-history-fragment?name={name}"),
            Scope::Team(team) => format!("/deploy-history-fragment?team={team}"),
            Scope::Teams(_) => "/deploy-history-fragment".to_string(),
        }
    }

    fn revisions(&self, conn: &PooledConnection<SqliteConnectionManager>) -> Vec<Revision> {
        let result = match self {
            Scope::Config(name) => Revision::list_for(conn, name, HISTORY_LIMIT),
            Scope::Team(team) => Revision::list_for_team(conn, team, HISTORY_LIMIT),
            Scope::Teams(teams) => {
                let mut acc = Vec::new();
                for team in teams {
                    match Revision::list_for_team(conn, team, HISTORY_LIMIT) {
                        Ok(mut revs) => acc.append(&mut revs),
                        Err(e) => log::error!("Failed to load revisions for team {}: {}", team, e),
                    }
                }
                acc.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
                acc.truncate(HISTORY_LIMIT);
                Ok(acc)
            }
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

fn render_page(conn: &PooledConnection<SqliteConnectionManager>, scope: &Scope) -> Markup {
    let title = scope.title();
    let revisions = scope.revisions(conn);
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (title) }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.deploy-history-page hx-ext="morph" {
                (header::render("history"))
                div class="content" {
                    header {
                        h1 { (title) }
                        div class="subtitle" { "Most recent first" }
                    }
                    @if revisions.is_empty() {
                        div class="empty-state" {
                            h2 { "No history found" }
                            p { "There are no revisions matching this filter." }
                        }
                    } @else {
                        (render_table(conn, revisions, &scope.fragment_url()))
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

#[get("/deploy-history/{name}")]
pub async fn deploy_history(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<String>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return db_error(e),
    };
    let scope = Scope::Config(path.into_inner());
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_page(&conn, &scope).into_string())
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
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_page(&conn, &scope).into_string())
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
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_rows(&conn, revisions).into_string())
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
}
