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
use crate::web::{deploy_form, header, Action};

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
