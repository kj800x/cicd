//! Render the deploy and blockers pages from synthetic data into static
//! HTML, for looking at the design without a cluster. Not a test of
//! anything; it runs only when asked:
//!
//! ```text
//! CICD_FIXTURE_DIR=/tmp/cicd-pages cargo test fixtures -- --ignored
//! ```
//!
//! Each file inlines the stylesheets and links the icon fonts from the
//! CDN, so it renders from disk or from any static server.

use std::collections::{BTreeMap, HashMap};

use kube::ResourceExt;
use maud::{html, Markup, DOCTYPE};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::blocker::Blocker;
use crate::db::git_branch::GitBranchEgg;
use crate::db::git_commit::{GitCommit, GitCommitEgg};
use crate::db::git_commit_build::GitCommitBuild;
use crate::db::git_repo::GitRepo;
use crate::db::revision::{NewRevision, Revision, RevisionParameter};
use crate::db::test_support::migrated_memory_pool;
use crate::error::AppResult;
use crate::kubernetes::deploy_config::{
    DeployConfigSpec, DeployConfigSpecFields, DeployConfigStatus, Template,
};
use crate::kubernetes::parameters::{ParameterSource, ParameterValue, SHA_PARAMETER};
use crate::kubernetes::patches::{ManifestPatch, PatchOp, PatchTarget};
use crate::kubernetes::repo::{Repository, ShaMaybeBranch};
use crate::kubernetes::selections::{Choice, Durability, Selection};
use crate::kubernetes::DeployConfig;
use crate::web::preview::{self, TagResolutions};
use crate::web::{deploy_form, deploy_history, feed, header, home, Action};

const OLD_SHA: &str = "a1b2c3d0e77a41cb9d2f8e3b7a05c164de9038aa";
const NEW_SHA: &str = "e4f5a6b0d33c9a17bb4419e0cf7a2b81de905cc4";
const FIX_SHA: &str = "9c31be7ab0c4d9e2f1a68035dd4b7c2190ef5a3b";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn ago(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms() - ms)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

fn seed(conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
    GitRepo {
        id: 1,
        owner_name: "kj800x".into(),
        name: "alldex-rs".into(),
        default_branch: "master".into(),
        private: false,
        language: Some("Rust".into()),
    }
    .upsert(conn)?;
    let master = GitBranchEgg {
        name: "master".into(),
        head_commit_sha: NEW_SHA.into(),
        repo_id: 1,
        active: true,
    }
    .upsert(conn)?;
    let fix = GitBranchEgg {
        name: "fix/upload-timeout".into(),
        head_commit_sha: FIX_SHA.into(),
        repo_id: 1,
        active: true,
    }
    .upsert(conn)?;
    let commits = [
        (
            OLD_SHA,
            "Bump nginx sidecar to 1.27.4",
            26 * 3_600_000,
            master.id,
        ),
        (
            NEW_SHA,
            "Paginate the search index rebuild",
            3 * 3_600_000,
            master.id,
        ),
        (FIX_SHA, "retry uploads on 504", 12 * 60_000, fix.id),
    ];
    for (sha, message, age, branch_id) in commits {
        let commit = GitCommit::upsert(
            &GitCommitEgg {
                sha: sha.into(),
                repo_id: 1,
                message: message.into(),
                author: "kevin".into(),
                committer: "kevin".into(),
                timestamp: now_ms() - age,
            },
            conn,
        )?;
        commit.add_branch(branch_id, conn)?;
        GitCommitBuild::upsert(
            &GitCommitBuild {
                repo_id: 1,
                commit_id: commit.id,
                check_name: "build".into(),
                status: "Success".into(),
                url: "https://github.com/kj800x/alldex-rs/actions".into(),
                start_time: Some((now_ms() - age) as u64),
                settle_time: Some((now_ms() - age + 240_000) as u64),
                app_id: None,
            },
            conn,
        )?;
    }
    Ok(())
}

fn manifests(name: &str) -> Vec<serde_json::Value> {
    let ns = "alldex";
    vec![
        Template::stored(
            "deployment.yaml",
            serde_json::json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": name, "namespace": ns},
                "spec": {"replicas": "$REPLICAS", "template": {"spec": {"containers": [
                    {"name": "app", "image": format!("ghcr.io/kj800x/{name}:commit-$SHA"),
                     "env": [{"name": "LOG_LEVEL", "value": "$LOG_LEVEL"}, {"name": "S3_ENDPOINT", "value": "old"}]},
                    {"name": "nginx", "image": "ghcr.io/kj800x/nginx:$NGINX"}
                ]}}}
            }),
        ),
        Template::stored(
            "service.yaml",
            serde_json::json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name, "namespace": ns}, "spec": {"type": "LoadBalancer"}}),
        ),
        Template::stored(
            "ingress.yaml",
            serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "Ingress", "metadata": {"name": name, "namespace": ns}}),
        ),
    ]
}

fn config(name: &str) -> DeployConfig {
    let mut parameters = BTreeMap::new();
    parameters.insert(
        SHA_PARAMETER.to_string(),
        ParameterSource::Commit {
            owner: "kj800x".into(),
            repo: "alldex-rs".into(),
            branch: "master".into(),
        },
    );
    parameters.insert(
        "NGINX".to_string(),
        ParameterSource::Tag {
            image: "ghcr.io/kj800x/nginx".into(),
            pattern: "1.27.*".into(),
            variant: None,
        },
    );
    parameters.insert(
        "REPLICAS".to_string(),
        ParameterSource::Value {
            default: "2".into(),
        },
    );
    parameters.insert(
        "LOG_LEVEL".to_string(),
        ParameterSource::Value {
            default: "info".into(),
        },
    );
    let mut dc = DeployConfig::new(
        name,
        DeployConfigSpec {
            spec: DeployConfigSpecFields {
                team: "cluster-infra".into(),
                kind: "service".into(),
                parameters,
                selections: Default::default(),
                patches: vec![],
                config: Repository {
                    owner: "kj800x".into(),
                    repo: "alldex-rs".into(),
                },
                specs: manifests(name),
            },
        },
    );
    dc.metadata.namespace = Some("alldex".into());
    let mut status = DeployConfigStatus {
        config: Some(ShaMaybeBranch {
            sha: OLD_SHA.into(),
            branch: Some("master".into()),
        }),
        autodeploy: Some(true),
        orphaned: Some(false),
        ..Default::default()
    };
    status.parameters.insert(
        SHA_PARAMETER.into(),
        ParameterValue::Commit {
            value: OLD_SHA.into(),
            branch: Some("master".into()),
        },
    );
    status.parameters.insert(
        "NGINX".into(),
        ParameterValue::Tag {
            value: "1.27.4".into(),
            pattern: Some("1.27.*".into()),
            digest: None,
        },
    );
    status.parameters.insert(
        "REPLICAS".into(),
        ParameterValue::Value { value: "2".into() },
    );
    status.parameters.insert(
        "LOG_LEVEL".into(),
        ParameterValue::Value {
            value: "info".into(),
        },
    );
    dc.status = Some(status);
    dc
}

/// The alldex-rs config as a temporary deployment: SHA tracking a fix
/// branch, NGINX pinned, one standing and one temporary patch.
fn advanced_state() -> DeployConfig {
    let mut dc = config("alldex-rs");
    let mut track = Selection::track("fix/upload-timeout", Durability::Temporary)
        .with_note(Some("advanced deploy"), Some("kevin"));
    track.since = Some(ago(2 * 3_600_000));
    dc.spec.spec.selections.insert(SHA_PARAMETER.into(), track);
    let mut pin = Selection::pin("1.27.4", Durability::Temporary);
    pin.since = Some(ago(2 * 3_600_000));
    dc.spec.spec.selections.insert("NGINX".into(), pin);
    dc.spec.spec.patches = vec![
        env_patch(
            Durability::Standing,
            "point at the new S3 endpoint",
            86_400_000,
        ),
        env_patch(Durability::Temporary, "load test", 2 * 3_600_000),
    ];
    if let Some(status) = dc.status.as_mut() {
        status.autodeploy = Some(false);
        status.parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::Commit {
                value: FIX_SHA.into(),
                branch: Some("fix/upload-timeout".into()),
            },
        );
        status.config = Some(ShaMaybeBranch {
            sha: NEW_SHA.into(),
            branch: Some("master".into()),
        });
    }
    dc
}

fn paperless_like_alldex(dc: &DeployConfig) -> DeployConfig {
    dc.clone()
}

fn env_patch(durability: Durability, note: &str, age_ms: i64) -> ManifestPatch {
    ManifestPatch {
        target: PatchTarget {
            file: Some("deployment.yaml".into()),
            kind: "Deployment".into(),
            name: "alldex-rs".into(),
        },
        op: PatchOp::Replace,
        path: "/spec/template/spec/containers/0/env/1/value".into(),
        value: Some(serde_json::json!("https://s3.new.example")),
        durability,
        note: Some(note.into()),
        by: Some("web".into()),
        since: Some(ago(age_ms)),
    }
}

fn tag(value: &str, pattern: Option<&str>) -> ParameterValue {
    ParameterValue::Tag {
        value: value.into(),
        pattern: pattern.map(String::from),
        digest: None,
    }
}

fn css() -> String {
    let styles = include_str!("../res/styles.css").replace("@import \"deploy.css\";", "");
    format!("{}\n{}", include_str!("../res/deploy.css"), styles)
}

fn document(body_class: &str, active: &str, strips: Markup, content: Markup) -> String {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "fixture" }
                link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/font-awesome/4.4.0/css/font-awesome.css";
                link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/octicons/3.1.0/octicons.css";
                style { (maud::PreEscaped(css())) }
            }
            body class=(body_class) {
                (header::render(active))
                (strips)
                (content)
            }
        }
    }
    .into_string()
}

struct Scenario<'a> {
    file: &'a str,
    config: DeployConfig,
    action: Action,
    resolved: TagResolutions,
}

async fn render_deploy(
    conn: &PooledConnection<SqliteConnectionManager>,
    all_configs: &[DeployConfig],
    scenario: Scenario<'_>,
) -> String {
    let prepared = preview::prepare(conn, &scenario.config, &scenario.action);
    let held = !Blocker::active_for(conn, &scenario.config.name_any())
        .unwrap_or_default()
        .is_empty();
    let query: HashMap<String, String> = HashMap::new();
    let typed = BTreeMap::new();
    let form = deploy_form::render(
        &scenario.config,
        all_configs,
        &prepared,
        &query,
        held,
        &scenario.resolved,
        conn,
    );
    let body =
        preview::render_preview_content(&prepared, conn, None, &[], &typed, &scenario.resolved)
            .await;
    let strips = html! {
        (crate::web::blockers::render_held_strip(&Blocker::all_active(conn).unwrap_or_default()))
        (crate::web::selections::render_temporary_strip(all_configs))
    };
    let content = html! {
        div class="content" {
            div class="content-container" {
                (form)
                div class="right-box" {
                    h1 { (prepared.action.title()) strong { (scenario.config.name_any()) } }
                    div class="preview-meta" {
                        span { "Namespace " a.mono href="#" { "alldex" } }
                        span.preview-meta__item { "Autodeploy " (preview::render_autodeploy_badge(scenario.config.autodeploy())) }
                    }
                    div.preview-content-poll-wrapper { (body) }
                }
            }
        }
    };
    let _ = scenario.file;
    document("deploy-page", "deploy", strips, content)
}

#[tokio::test]
#[ignore]
async fn write_pages() -> AppResult<()> {
    let Ok(dir) = std::env::var("CICD_FIXTURE_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&dir)?;
    let pool = migrated_memory_pool();
    let conn = pool.get()?;
    seed(&conn)?;

    // Blockers: grafana held twice, one cleared hold in the history.
    Blocker::create(&conn, "grafana", "incident 42, do not deploy", "web")?;
    Blocker::create(
        &conn,
        "grafana",
        "waiting on the 11.2 dashboard migration",
        "web",
    )?;
    let old = Blocker::create(&conn, "plex", "waiting on the 1.41 transcoder fix", "web")?;
    Blocker::clear(&conn, old.id, "web")?;

    let simple = config("alldex-rs");

    let mut advanced = config("alldex-rs");
    advanced.spec.spec.patches = vec![env_patch(
        Durability::Standing,
        "point at the new S3 endpoint",
        6 * 3_600_000,
    )];
    let mut kept = env_patch(Durability::Standing, "keep the old bucket", 3 * 86_400_000);
    kept.path = "/spec/template/spec/containers/0/env/0/value".into();
    kept.value = Some(serde_json::json!("debug"));
    advanced.spec.spec.patches.push(kept);
    let mut pending_replicas = env_patch(Durability::Temporary, "load test", 0);
    pending_replicas.path = "/spec/replicas".into();
    pending_replicas.value = Some(serde_json::json!(3));
    pending_replicas.note = None;
    pending_replicas.since = None;
    let pending = crate::kubernetes::patches::PatchChanges {
        remove: vec![0],
        add: vec![pending_replicas],
    };
    if let Some(status) = advanced.status.as_mut() {
        status.autodeploy = Some(false);
    }

    let mut grafana = config("grafana");
    if let Some(status) = grafana.status.as_mut() {
        status.autodeploy = Some(false);
    }

    let mut paperless = config("paperless");
    let mut track = Selection::track("fix/upload-timeout", Durability::Temporary)
        .with_note(Some("waiting on the OCR fix"), Some("web"));
    track.since = Some(ago(2 * 3_600_000));
    paperless
        .spec
        .spec
        .selections
        .insert(SHA_PARAMETER.into(), track);
    paperless.spec.spec.patches = vec![env_patch(Durability::Temporary, "debug", 40 * 60_000)];
    if let Some(status) = paperless.status.as_mut() {
        status.parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::Commit {
                value: FIX_SHA.into(),
                branch: Some("fix/upload-timeout".into()),
            },
        );
    }

    let all = vec![simple.clone(), grafana.clone(), paperless.clone()];
    let mut ok = TagResolutions::new();
    ok.insert("NGINX".into(), Ok(tag("1.27.5", Some("1.27.*"))));
    let mut pinned = TagResolutions::new();
    pinned.insert("NGINX".into(), Ok(tag("1.27.4", None)));
    let mut down = TagResolutions::new();
    down.insert("NGINX".into(), Err("watchtower at http://watchtower.cicd.svc is unreachable: error sending request. alldex-rs cannot resolve NGINX (ghcr.io/kj800x/nginx, 1.27.*); type the tag to deploy for this once, or pin it".into()));

    let scenarios = vec![
        Scenario {
            file: "deploy-simple.html",
            config: simple.clone(),
            action: Action::DeployLatest,
            resolved: ok.clone(),
        },
        Scenario {
            file: "deploy-advanced.html",
            config: advanced,
            action: Action::DeployAdvanced {
                choices: BTreeMap::from([
                    (
                        SHA_PARAMETER.to_string(),
                        Choice::Track("fix/upload-timeout".into()),
                    ),
                    ("NGINX".to_string(), Choice::Pin("1.27.4".into())),
                    ("REPLICAS".to_string(), Choice::Default),
                    ("LOG_LEVEL".to_string(), Choice::Default),
                ]),
                durability: Durability::Temporary,
                patches: pending,
            },
            resolved: pinned,
        },
        Scenario {
            file: "deploy-blocked.html",
            config: grafana,
            action: Action::DeployLatest,
            resolved: ok.clone(),
        },
        Scenario {
            file: "deploy-watchtower-down.html",
            config: simple.clone(),
            action: Action::DeployLatest,
            resolved: down,
        },
        Scenario {
            file: "deploy-undeploy.html",
            config: simple.clone(),
            action: Action::Undeploy,
            resolved: TagResolutions::new(),
        },
        Scenario {
            file: "deploy-temporary.html",
            config: paperless,
            action: Action::DeployLatest,
            resolved: ok,
        },
    ];
    for scenario in scenarios {
        let file = scenario.file.to_string();
        let html = render_deploy(&conn, &all, scenario).await;
        std::fs::write(format!("{dir}/{file}"), html)?;
    }

    // Revisions: a day of activity across the configs, oldest first.
    let param = |name: &str, kind: &str, value: &str, branch: Option<&str>| RevisionParameter {
        name: name.into(),
        kind: kind.into(),
        value: value.into(),
        branch: branch.map(String::from),
    };
    let patch_json = |patches: &[ManifestPatch]| serde_json::to_string(patches).ok();
    let standing_patch = env_patch(Durability::Standing, "point at the new S3 endpoint", 0);
    let rev = |config: &str,
               actor: &str,
               action: &str,
               reason: Option<&str>,
               config_sha: &str,
               params: Vec<RevisionParameter>,
               patches: Option<String>,
               temporary: bool| NewRevision {
        config_name: config.into(),
        actor: actor.into(),
        action: action.into(),
        reason: reason.map(String::from),
        config_sha: Some(config_sha.into()),
        config_branch: Some("master".into()),
        config_version_hash: None,
        patches,
        temporary,
        parameters: params,
    };
    conn.execute(
        "INSERT INTO deploy_config (name, team, kind, config_repo_id, artifact_repo_id, active) VALUES ('alldex-rs','infra','service',1,1,1), ('grafana','infra','service',1,1,1), ('paperless','media','service',1,1,1), ('plex','media','service',1,1,1)",
        [],
    )?;
    let seeds = vec![
        (0, rev("alldex-rs", "web", "deploy", None, OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("NGINX", "tag", "1.27.4", Some("1.27.*"))], None, false)),
        (0, rev("alldex-rs", "kevin", "deploy", None, OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("NGINX", "tag", "1.27.4", Some("1.27.*"))], None, false)),
        (0, rev("alldex-rs", "kevin", "patch", Some("added patch: standing deployment.yaml:Deployment/alldex-rs replace /spec/template/spec/containers/0/env/1/value"), OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("NGINX", "tag", "1.27.4", Some("1.27.*"))], patch_json(&[standing_patch.clone()]), false)),
        (0, rev("grafana", "web", "deploy", None, OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("GRAFANA", "tag", "11.2.0", Some("11.*"))], None, false)),
        (1, rev("paperless", "autodeploy", "deploy", None, OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master"))], None, false)),
        (1, rev("plex", "autodeploy", "deploy", None, OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("PLEX", "tag", "1.41.2", Some("1.41.*"))], None, false)),
        (1, rev("paperless", "autodeploy", "deploy", None, NEW_SHA, vec![param("SHA", "commit", NEW_SHA, Some("master"))], None, false)),
        (1, rev("plex", "autodeploy", "deploy", None, OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("PLEX", "tag", "1.41.3", Some("1.41.*"))], None, false)),
        (1, rev("alldex-rs", "kevin", "deploy", None, NEW_SHA, vec![param("SHA", "commit", FIX_SHA, Some("fix/upload-timeout")), param("NGINX", "tag", "1.27.4", None)], patch_json(&[standing_patch.clone(), env_patch(Durability::Temporary, "load test", 0)]), true)),
        (1, rev("grafana", "web", "deploy", Some("rollback to revision 4: 11.2 dashboards broke the ZFS board"), OLD_SHA, vec![param("SHA", "commit", OLD_SHA, Some("master")), param("GRAFANA", "tag", "11.1.4", None)], None, false)),
    ];
    let mut ids = Vec::new();
    for (i, (_day, new)) in seeds.into_iter().enumerate() {
        let recorded = Revision::record(&conn, new)?;
        // Spread the revisions over yesterday and today.
        // Yesterday's four spread out; today's autodeploys a few minutes
        // apart so they read as a burst, then the two human deploys.
        let at = match i {
            0..=3 => now_ms() - 86_400_000 + (i as i64) * 3_600_000 * 2,
            4..=7 => now_ms() - 60 * 60_000 + (i as i64 - 4) * 4 * 60_000,
            8 => now_ms() - 20 * 60_000,
            _ => now_ms() - 5 * 60_000,
        };
        conn.execute(
            "UPDATE revision SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![at, recorded.id],
        )?;
        ids.push(recorded.id);
    }

    // Redeploy previous needs the revisions: alldex-rs's latest is the
    // temporary branch deploy, so the previous deployment is master.
    let redeploy = render_deploy(
        &conn,
        &all,
        Scenario {
            file: "deploy-redeploy-previous.html",
            config: simple.clone(),
            action: Action::RedeployPrevious { revision: None },
            resolved: TagResolutions::new(),
        },
    )
    .await;
    std::fs::write(format!("{dir}/deploy-redeploy-previous.html"), redeploy)?;

    let page_css = |markup: String| {
        markup.replace(
            "<link rel=\"stylesheet\" href=\"/res/styles.css\">",
            &format!("<style>{}</style>", css()),
        )
    };
    let strips = html! {
        (crate::web::blockers::render_held_strip(&Blocker::all_active(&conn)?))
        (crate::web::selections::render_temporary_strip(&all))
    };
    let scope = deploy_history::Scope::Teams(vec!["infra".into(), "media".into()]);
    std::fs::write(
        format!("{dir}/history-feed.html"),
        page_css(deploy_history::render_feed_page(&conn, &scope, strips.clone()).into_string()),
    )?;
    let grafana_blockers = Blocker::active_for(&conn, "grafana")?;
    let config_page = deploy_history::render_config_page(
        &conn,
        "alldex-rs",
        Some(&paperless_like_alldex(&advanced_state())),
        &[],
        strips.clone(),
    )
    .into_string();
    // Open the roll-back panel for the second newest deploy revision.
    let target = Revision::list_for(&conn, "alldex-rs", 10)?
        .into_iter()
        .filter(|r| r.action == "deploy")
        .nth(1);
    let panel = target
        .map(|t| deploy_history::render_rollback_panel(&advanced_state(), &t).into_string())
        .unwrap_or_default();
    std::fs::write(
        format!("{dir}/history-config.html"),
        page_css(config_page.replace(
            "<div id=\"rollback-panel\"></div>",
            &format!("<div id=\"rollback-panel\">{panel}</div>"),
        )),
    )?;
    let _ = grafana_blockers;
    if let Some(last) = ids.last() {
        let revision =
            Revision::get(&conn, *last)?.ok_or(crate::error::AppError::NotFound("rev".into()))?;
        let previous = revision.previous(&conn)?;
        std::fs::write(
            format!("{dir}/revision.html"),
            page_css(
                deploy_history::render_revision_page(
                    &conn,
                    &revision,
                    previous.as_ref(),
                    strips.clone(),
                )
                .into_string(),
            ),
        )?;
    }

    // Home, populated and quiet.
    let revisions = deploy_history::revisions_for_teams(&conn, &["infra".into(), "media".into()])?;
    let mut activity = feed::group_bursts(feed::entries(revisions));
    activity.truncate(6);
    let mut home_configs = all.clone();
    home_configs.push(advanced_state());
    let data = home::HomeData {
        configs: home_configs.clone(),
        blockers: Blocker::all_active(&conn)?,
        unhealthy: vec![home::Unhealthy { name: "lerke".into(), message: "Deployment lerke: Ready 0 / 2 · CrashLoopBackOff".into(), since: Some(now_ms() - 11 * 3_600_000) }],
        healthy_count: home_configs.len() - 1,
        drift: vec![
            home::Drift { name: "alldex-rs".into(), lines: vec![
                home::DriftLine::Moves { name: "SHA".into(), from: "9c31be7".into(), to: "2f9ab30".into(), channel: "fix/upload-timeout".into() },
                home::DriftLine::HeldBack { name: "NGINX".into(), value: "1.27.4".into(), available: "1.27.6".into() },
            ], autodeploy: false, temporary: true, pinned: true },
            home::Drift { name: "nginx-cache".into(), lines: vec![
                home::DriftLine::HeldBack { name: "NGINX".into(), value: "1.27.0".into(), available: "1.27.6".into() },
            ], autodeploy: true, temporary: false, pinned: true },
        ],
        building: vec![
            home::ActiveBuild {
                configs: vec!["alldex-rs".into(), "alldex-worker".into()],
                sha: "b7d21e0".into(),
                channel: "master".into(),
                message: "Cache the species index between requests".into(),
                author: "kevin".into(),
                committed_at: now_ms() - 5 * 60_000,
                build_url: Some("https://github.com/kj800x/alldex-rs/actions".into()),
                elapsed_ms: Some(4 * 60_000 + 20_000),
                pct: Some(43),
                remaining_ms: Some(5 * 60_000 + 40_000),
            },
            home::ActiveBuild {
                configs: vec!["lerke".into()],
                sha: "0c4f9aa".into(),
                channel: "fix/retry-uploads".into(),
                message: "Retry uploads on 504".into(),
                author: "kevin".into(),
                committed_at: now_ms() - 40_000,
                build_url: None,
                elapsed_ms: None,
                pct: None,
                remaining_ms: None,
            },
        ],
        upgrades: vec![
            home::Upgrade { name: "nginx-cache".into(), config_url: "https://github.com/kj800x/homelab/blob/master/.deploy/nginx-cache.yaml".into(), lines: vec![
                home::UpgradeLine { name: "NGINX".into(), current: "1.27.0".into(), newest: "1.29.1".into(), channel: "1.27.*".into() },
            ] },
            home::Upgrade { name: "mosquitto".into(), config_url: "https://github.com/kj800x/homelab/blob/master/.deploy/mosquitto.yaml".into(), lines: vec![
                home::UpgradeLine { name: "MQTT".into(), current: "2.1.2-alpine".into(), newest: "3.0.1-alpine".into(), channel: "^2.1.2 · alpine".into() },
            ] },
        ],
        activity: activity.clone(),
        standing: vec![
            home::Standing { name: "nginx-cache".into(), what: "NGINX pinned 1.27.0".into(), since: Some(now_ms() - 86 * 86_400_000) },
            home::Standing { name: "alldex-rs".into(), what: "patch: replace /spec/template/spec/containers/0/env/1/value on Deployment/alldex-rs".into(), since: Some(now_ms() - 86_400_000) },
        ],
        last_deploy: Some(now_ms() - 20 * 60_000),
        teams: vec!["infra".into(), "media".into()],
        selected_team: None,
    };
    std::fs::write(
        format!("{dir}/home.html"),
        page_css(home::render_home(&data, strips.clone(), true).into_string()),
    )?;
    let quiet = home::HomeData {
        configs: home_configs.clone(),
        blockers: vec![],
        unhealthy: vec![],
        healthy_count: home_configs.len(),
        drift: vec![],
        building: vec![],
        upgrades: vec![],
        activity,
        standing: data.standing,
        last_deploy: data.last_deploy,
        teams: data.teams,
        selected_team: None,
    };
    let mut quiet_configs = quiet.configs.clone();
    quiet_configs.retain(|c| !c.is_temporary_deployment());
    let healthy_count = quiet_configs.len();
    let quiet = home::HomeData {
        configs: quiet_configs,
        healthy_count,
        ..quiet
    };
    std::fs::write(
        format!("{dir}/home-quiet.html"),
        page_css(home::render_home(&quiet, html! {}, true).into_string()),
    )?;

    let active = Blocker::all_active(&conn)?;
    let cleared = Blocker::recently_cleared(&conn, 20)?;
    let names: Vec<String> = all.iter().map(|c| c.name_any()).collect();
    let page = crate::web::blockers::render_blockers_page(&active, &cleared, &names)
        .into_string()
        .replace(
            "<link rel=\"stylesheet\" href=\"/res/styles.css\">",
            &format!("<style>{}</style>", css()),
        );
    std::fs::write(format!("{dir}/blockers.html"), page)?;
    Ok(())
}
