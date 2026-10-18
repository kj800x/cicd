//! Deploy checks: the one small HTTP protocol cicd speaks to ask other
//! services whether a deploy may go ahead.
//!
//! cicd knows nothing about Jellyfin or any other service. A check is a
//! name and a URL in the config's `.deploy` file; cicd POSTs a JSON
//! request describing the deploy and reads back a status:
//!
//! ```json
//! {"version": 1, "phase": "pre", "mode": "gate", "runId": 412,
//!  "check": "jellyfin-streams",
//!  "config": {"name": "jellyfin", "namespace": "media", "kind": "service"},
//!  "trigger": "autodeploy", "actor": "autodeploy",
//!  "from": {"parameters": {"SHA": "9f1c…"}},
//!  "to": {"parameters": {"SHA": "a07e…"}},
//!  "attempt": 1, "startedAt": "2026-10-17T18:02:11Z"}
//! ```
//!
//! ```json
//! {"status": "SOFT_BLOCK", "title": "2 active streams",
//!  "message": "alice on Living Room TV", "detailsUrl": "https://…"}
//! ```
//!
//! A pre-check answers `OK`, `SOFT_BLOCK` (a person may acknowledge it and
//! deploy anyway) or `HARD_BLOCK`. `mode` is `preview` when the deploy
//! page asks and `gate` when a deploy is about to be applied. `runId` is
//! the waiting run being asked about, absent for a first ask. A check that
//! times out, errors, answers non-2xx or answers something unreadable
//! counts as a `SOFT_BLOCK` titled "check X didn't respond": a broken check
//! holds autodeploys but never locks a person out.
//!
//! Verify and watch checks (together, post-checks here) are asked once a
//! deploy has rolled out (phase `verify` or `watch`, mode `monitor`,
//! `to.revision` and `rolledOutAt` set) and answer `OK`, `WARN`,
//! `ROLLBACK` or `PENDING` with `retryAfter` seconds; cicd asks again with
//! the same `runId` until the answer is final or the check's timeout
//! passes. A `job` check runs a jobfather JobTemplate instead and passes
//! when jobfather says the job passed.
use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use futures_util::future::join_all;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client, ResourceExt};
use serde::{Deserialize, Serialize};

use crate::kubernetes::deploy_config::{parse_duration_ms, CheckSpec, Severity};
use crate::kubernetes::DeployConfig;

/// How long a check may take unless its spec says otherwise.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The deploy page asking what a deploy would meet.
    Preview,
    /// A deploy about to be applied.
    Gate,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Preview => "preview",
            Mode::Gate => "gate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PreStatus {
    Ok,
    SoftBlock,
    HardBlock,
}

impl PreStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PreStatus::Ok => "OK",
            PreStatus::SoftBlock => "SOFT_BLOCK",
            PreStatus::HardBlock => "HARD_BLOCK",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OK" => Some(PreStatus::Ok),
            "SOFT_BLOCK" => Some(PreStatus::SoftBlock),
            "HARD_BLOCK" => Some(PreStatus::HardBlock),
            _ => None,
        }
    }
}

/// What one pre-check said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreOutcome {
    pub name: String,
    pub status: PreStatus,
    pub title: Option<String>,
    pub message: Option<String>,
    pub details_url: Option<String>,
    /// Milliseconds since the epoch.
    pub started_at: i64,
    pub finished_at: i64,
}

impl PreOutcome {
    /// `name: title` for refusals and notifications.
    pub fn summary(&self) -> String {
        match &self.title {
            Some(title) => format!("{}: {}", self.name, title),
            None => format!("{}: {}", self.name, self.status.as_str()),
        }
    }
}

/// Everything about the deploy a check is told, apart from which check
/// it is.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub run_id: Option<i64>,
    pub trigger: String,
    pub actor: String,
    pub from: BTreeMap<String, String>,
    /// What the deploy would apply; empty for a preview that does not know.
    pub to: BTreeMap<String, String>,
    pub attempt: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a> {
    version: u32,
    phase: &'static str,
    mode: &'static str,
    run_id: Option<i64>,
    check: &'a str,
    config: ConfigRef,
    trigger: &'a str,
    actor: &'a str,
    from: Side<'a>,
    to: Side<'a>,
    attempt: i64,
    started_at: String,
    /// Verify and watch checks: when the run rolled out, where a window
    /// such as "no new alerts for 15 minutes" starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    rolled_out_at: Option<String>,
}

#[derive(Serialize)]
struct ConfigRef {
    name: String,
    namespace: String,
    kind: String,
}

#[derive(Serialize)]
struct Side<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<i64>,
    parameters: &'a BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    status: String,
    title: Option<String>,
    message: Option<String>,
    details_url: Option<String>,
    retry_after: Option<u64>,
}

/// Ask every pre-check of `config` at once. The order of the result
/// follows the config.
pub async fn ask_pre_checks(
    client: &Client,
    config: &DeployConfig,
    mode: Mode,
    ask: &Ask,
) -> Vec<PreOutcome> {
    let specs = &config.spec.spec.checks.pre;
    join_all(
        specs
            .iter()
            .map(|spec| ask_pre_check(client, config, spec, mode, ask)),
    )
    .await
}

async fn ask_pre_check(
    client: &Client,
    config: &DeployConfig,
    spec: &CheckSpec,
    mode: Mode,
    ask: &Ask,
) -> PreOutcome {
    let started_at = Utc::now().timestamp_millis();
    let answer = post(client, config, spec, mode, ask).await;
    let finished_at = Utc::now().timestamp_millis();
    match answer {
        Ok(response) => match PreStatus::parse(&response.status) {
            Some(status) => PreOutcome {
                name: spec.name.clone(),
                status,
                title: response.title,
                message: response.message,
                details_url: response.details_url,
                started_at,
                finished_at,
            },
            None => no_answer(
                spec,
                format!("unknown status {:?}", response.status),
                started_at,
                finished_at,
            ),
        },
        Err(why) => no_answer(spec, why, started_at, finished_at),
    }
}

fn no_answer(spec: &CheckSpec, why: String, started_at: i64, finished_at: i64) -> PreOutcome {
    log::warn!("Check {} didn't respond: {}", spec.name, why);
    PreOutcome {
        name: spec.name.clone(),
        status: PreStatus::SoftBlock,
        title: Some(format!("check {} didn't respond", spec.name)),
        message: Some(why),
        details_url: None,
        started_at,
        finished_at,
    }
}

async fn post(
    client: &Client,
    config: &DeployConfig,
    spec: &CheckSpec,
    mode: Mode,
    ask: &Ask,
) -> Result<Response, String> {
    let url = spec
        .http
        .as_deref()
        .ok_or_else(|| "the check has no http URL".to_string())?;
    let timeout = spec
        .timeout
        .as_deref()
        .and_then(parse_duration_ms)
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(DEFAULT_TIMEOUT);
    let namespace = config.namespace().unwrap_or_else(|| "default".to_string());
    let body = Request {
        version: 1,
        phase: "pre",
        mode: mode.as_str(),
        run_id: ask.run_id,
        check: &spec.name,
        config: ConfigRef {
            name: config.name_any(),
            namespace: namespace.clone(),
            kind: config.spec.spec.kind.clone(),
        },
        trigger: &ask.trigger,
        actor: &ask.actor,
        from: Side {
            revision: None,
            parameters: &ask.from,
        },
        to: Side {
            revision: None,
            parameters: &ask.to,
        },
        attempt: ask.attempt,
        started_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        rolled_out_at: None,
    };
    let mut request = reqwest::Client::new()
        .post(url)
        .timeout(timeout)
        .json(&body);
    if let Some(secret_ref) = &spec.secret_ref {
        let token = read_secret(client, &namespace, &secret_ref.name, &secret_ref.key).await?;
        request = request.bearer_auth(token.trim());
    }
    let response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            format!("no answer within {timeout:?}")
        } else {
            e.to_string()
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("answered HTTP {status}"));
    }
    response
        .json::<Response>()
        .await
        .map_err(|e| format!("unreadable answer: {e}"))
}

async fn read_secret(
    client: &Client,
    namespace: &str,
    name: &str,
    key: &str,
) -> Result<String, String> {
    let secret = Api::<Secret>::namespaced(client.clone(), namespace)
        .get(name)
        .await
        .map_err(|e| format!("could not read Secret {name}: {e}"))?;
    let bytes = secret
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .ok_or_else(|| format!("Secret {name} has no key {key}"))?;
    String::from_utf8(bytes.0.clone()).map_err(|_| format!("Secret {name} key {key} is not text"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostStatus {
    Ok,
    Warn,
    Rollback,
    Pending,
}

impl PostStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PostStatus::Ok => "OK",
            PostStatus::Warn => "WARN",
            PostStatus::Rollback => "ROLLBACK",
            PostStatus::Pending => "PENDING",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OK" => Some(PostStatus::Ok),
            "WARN" => Some(PostStatus::Warn),
            "ROLLBACK" => Some(PostStatus::Rollback),
            "PENDING" => Some(PostStatus::Pending),
            _ => None,
        }
    }

    /// How bad a final answer is; `None` while pending.
    pub fn severity(self) -> Option<Severity> {
        match self {
            PostStatus::Ok => Some(Severity::Ok),
            PostStatus::Warn => Some(Severity::Warn),
            PostStatus::Rollback => Some(Severity::Rollback),
            PostStatus::Pending => None,
        }
    }

    pub fn from_severity(severity: Severity) -> Self {
        match severity {
            Severity::Ok => PostStatus::Ok,
            Severity::Warn => PostStatus::Warn,
            Severity::Rollback => PostStatus::Rollback,
        }
    }
}

/// One answer from a post-check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostPoll {
    pub status: PostStatus,
    pub title: Option<String>,
    pub message: Option<String>,
    pub details_url: Option<String>,
    /// When a pending check wants to be asked again, in seconds.
    pub retry_after: Option<u64>,
    /// What the check started (a jobfather job's name), to remember.
    pub handle: Option<String>,
}

/// The run a verify or watch check is asked about.
#[derive(Clone, Debug, Default)]
pub struct PostAsk {
    /// `verify` or `watch`.
    pub phase: &'static str,
    pub run_id: i64,
    /// When the run rolled out, in milliseconds.
    pub rolled_out_at: Option<i64>,
    pub revision_id: Option<i64>,
    pub trigger: String,
    pub parameters: BTreeMap<String, String>,
    pub attempt: i64,
    /// What an earlier poll started, if anything.
    pub handle: Option<String>,
}

/// How long one post-check request may take; the check's own `timeout`
/// is its overall deadline.
const POST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Where jobfather's API is, from inside the cluster.
fn jobfather_url() -> String {
    std::env::var("JOBFATHER_URL")
        .unwrap_or_else(|_| "http://jobfather.cicd.svc".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// Where a person sees a jobfather job.
const JOBFATHER_UI: &str = "https://jobs.home.coolkev.com";

/// Ask one post-check once. An error is a check that did not answer this
/// time; the caller keeps asking until the deadline.
pub async fn poll_post_check(
    client: &Client,
    config: &DeployConfig,
    spec: &CheckSpec,
    ask: &PostAsk,
) -> Result<PostPoll, String> {
    match (&spec.http, &spec.job) {
        (Some(url), _) => poll_http(client, config, spec, url, ask).await,
        (None, Some(job)) => poll_job(job, ask).await,
        (None, None) => Err("the check has neither http nor job".to_string()),
    }
}

async fn poll_http(
    client: &Client,
    config: &DeployConfig,
    spec: &CheckSpec,
    url: &str,
    ask: &PostAsk,
) -> Result<PostPoll, String> {
    let namespace = config.namespace().unwrap_or_else(|| "default".to_string());
    let empty = BTreeMap::new();
    let body = Request {
        version: 1,
        phase: ask.phase,
        mode: "monitor",
        run_id: Some(ask.run_id),
        check: &spec.name,
        config: ConfigRef {
            name: config.name_any(),
            namespace: namespace.clone(),
            kind: config.spec.spec.kind.clone(),
        },
        trigger: &ask.trigger,
        actor: &ask.trigger,
        from: Side {
            revision: None,
            parameters: &empty,
        },
        to: Side {
            revision: ask.revision_id,
            parameters: &ask.parameters,
        },
        attempt: ask.attempt,
        started_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        rolled_out_at: ask
            .rolled_out_at
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    };
    let mut request = reqwest::Client::new()
        .post(url)
        .timeout(POST_REQUEST_TIMEOUT)
        .json(&body);
    if let Some(secret_ref) = &spec.secret_ref {
        let token = read_secret(client, &namespace, &secret_ref.name, &secret_ref.key).await?;
        request = request.bearer_auth(token.trim());
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("answered HTTP {status}"));
    }
    let response: Response = response
        .json()
        .await
        .map_err(|e| format!("unreadable answer: {e}"))?;
    let status = PostStatus::parse(&response.status)
        .ok_or_else(|| format!("unknown status {:?}", response.status))?;
    Ok(PostPoll {
        status,
        title: response.title,
        message: response.message,
        details_url: response.details_url,
        retry_after: response.retry_after,
        handle: None,
    })
}

#[derive(Deserialize)]
struct StartedJob {
    name: String,
}

#[derive(Deserialize)]
struct JobState {
    status: String,
    passed: Option<bool>,
}

async fn poll_job(job: &str, ask: &PostAsk) -> Result<PostPoll, String> {
    let (namespace, template) = job
        .split_once('/')
        .ok_or_else(|| format!("job {job:?} is not namespace/name"))?;
    let http = reqwest::Client::new();
    let Some(name) = &ask.handle else {
        // First poll: start the job, telling it which deploy it checks.
        let mut env: BTreeMap<String, String> = ask
            .parameters
            .iter()
            .map(|(k, v)| (format!("CICD_PARAM_{k}"), v.clone()))
            .collect();
        env.insert("CICD_RUN_ID".to_string(), ask.run_id.to_string());
        if let Some(revision) = ask.revision_id {
            env.insert("CICD_REVISION".to_string(), revision.to_string());
        }
        let started: StartedJob = http
            .post(format!(
                "{}/api/job-templates/{namespace}/{template}/run",
                jobfather_url()
            ))
            .timeout(POST_REQUEST_TIMEOUT)
            .json(&serde_json::json!({ "env": env }))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("could not start {job}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("unreadable answer starting {job}: {e}"))?;
        return Ok(PostPoll {
            status: PostStatus::Pending,
            title: Some(format!("started {}", started.name)),
            message: None,
            details_url: Some(format!("{JOBFATHER_UI}/jobs/{namespace}/{}", started.name)),
            retry_after: Some(15),
            handle: Some(started.name),
        });
    };
    let state: JobState = http
        .get(format!("{}/api/jobs/{namespace}/{name}", jobfather_url()))
        .timeout(POST_REQUEST_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("could not read job {name}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("unreadable job {name}: {e}"))?;
    let (status, title) = match state.passed {
        None => (
            PostStatus::Pending,
            format!("job {}", state.status.to_lowercase()),
        ),
        Some(true) => (PostStatus::Ok, "job passed".to_string()),
        Some(false) => (
            PostStatus::Rollback,
            format!("job failed ({})", state.status.to_lowercase()),
        ),
    };
    Ok(PostPoll {
        status,
        title: Some(title),
        message: None,
        details_url: Some(format!("{JOBFATHER_UI}/jobs/{namespace}/{name}")),
        retry_after: Some(15),
        handle: Some(name.clone()),
    })
}

/// The checks that hold a deploy: every hard block, and every soft block
/// not in `acknowledged`.
pub fn holding<'a>(outcomes: &'a [PreOutcome], acknowledged: &[String]) -> Vec<&'a PreOutcome> {
    outcomes
        .iter()
        .filter(|o| match o.status {
            PreStatus::Ok => false,
            PreStatus::SoftBlock => !acknowledged.iter().any(|a| a == &o.name),
            PreStatus::HardBlock => true,
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::kubernetes::deploy_config::{
        DeployChecks, DeployConfigSpec, DeployConfigSpecFields,
    };
    use crate::kubernetes::repo::Repository;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-shot HTTP server that answers every request with `status` and
    /// `body`, and hands back the request it saw.
    pub(crate) async fn serve(
        status: &'static str,
        body: &'static str,
        delay_ms: u64,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/check", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 16384];
            let mut seen = String::new();
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some((head, rest)) = seen.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            seen
        });
        (url, handle)
    }

    pub(crate) fn config(checks: Vec<CheckSpec>) -> DeployConfig {
        let mut dc = DeployConfig::new(
            "jellyfin",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    checks: DeployChecks {
                        pre: checks,
                        ..Default::default()
                    },
                    team: "media".into(),
                    kind: "service".into(),
                    parameters: Default::default(),
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "kj800x".into(),
                        repo: "homelab".into(),
                    },
                    specs: vec![],
                },
            },
        );
        dc.metadata.namespace = Some("media".into());
        dc
    }

    pub(crate) fn check(name: &str, url: &str, timeout: Option<&str>) -> CheckSpec {
        CheckSpec {
            name: name.into(),
            http: Some(url.into()),
            timeout: timeout.map(String::from),
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn a_watch_check_reads_pending_and_retry_after() {
        let (url, seen) = serve(
            "200 OK",
            r#"{"status":"PENDING","title":"quiet for 3 of 10 minutes","retryAfter":30}"#,
            0,
        )
        .await;
        let ask = PostAsk {
            phase: "watch",
            run_id: 9,
            rolled_out_at: Some(1_760_724_000_000),
            revision_id: Some(712),
            trigger: "autodeploy".into(),
            parameters: BTreeMap::from([("SHA".into(), "new".into())]),
            attempt: 1,
            handle: None,
        };
        let spec = check("quiet", &url, None);
        let poll = poll_post_check(&unused_client(), &config(vec![]), &spec, &ask)
            .await
            .expect("an answer");
        assert_eq!(poll.status, PostStatus::Pending);
        assert_eq!(poll.retry_after, Some(30));
        let request = seen.await.expect("server task");
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("json body");
        assert_eq!(body["phase"], "watch");
        assert_eq!(body["rolledOutAt"], "2025-10-17T18:00:00Z");
        assert_eq!(body["mode"], "monitor");
        assert_eq!(body["runId"], 9);
        assert_eq!(body["to"]["revision"], 712);
        assert_eq!(body["to"]["parameters"]["SHA"], "new");

        let (bad, _s) = serve("200 OK", r#"{"status":"SOFT_BLOCK"}"#, 0).await;
        let err = poll_post_check(
            &unused_client(),
            &config(vec![]),
            &check("quiet", &bad, None),
            &ask,
        )
        .await;
        assert_eq!(err, Err("unknown status \"SOFT_BLOCK\"".to_string()));
    }

    /// A client that is never used: these checks carry no secretRef.
    pub(crate) fn unused_client() -> Client {
        let config = kube::Config::new("http://127.0.0.1:1".parse().expect("uri"));
        Client::try_from(config).expect("client")
    }

    #[tokio::test]
    async fn a_check_is_asked_about_the_deploy_and_its_answer_read() {
        let (url, seen) = serve(
            "200 OK",
            r#"{"status":"SOFT_BLOCK","title":"2 active streams","message":"alice","detailsUrl":"https://j/x"}"#,
            0,
        )
        .await;
        let ask = Ask {
            run_id: Some(7),
            trigger: "autodeploy".into(),
            actor: "autodeploy".into(),
            from: BTreeMap::from([("SHA".into(), "old".into())]),
            to: BTreeMap::from([("SHA".into(), "new".into())]),
            attempt: 2,
        };
        let outcomes = ask_pre_checks(
            &unused_client(),
            &config(vec![check("streams", &url, None)]),
            Mode::Gate,
            &ask,
        )
        .await;
        assert_eq!(outcomes.len(), 1);
        let o = &outcomes[0];
        assert_eq!(o.status, PreStatus::SoftBlock);
        assert_eq!(o.title.as_deref(), Some("2 active streams"));
        assert_eq!(o.details_url.as_deref(), Some("https://j/x"));
        assert_eq!(o.summary(), "streams: 2 active streams");

        let request = seen.await.expect("server task");
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("json body");
        assert_eq!(body["version"], 1);
        assert_eq!(body["phase"], "pre");
        assert_eq!(body["mode"], "gate");
        assert_eq!(body["runId"], 7);
        assert_eq!(body["check"], "streams");
        assert_eq!(body["config"]["name"], "jellyfin");
        assert_eq!(body["config"]["namespace"], "media");
        assert_eq!(body["from"]["parameters"]["SHA"], "old");
        assert_eq!(body["to"]["parameters"]["SHA"], "new");
        assert_eq!(body["attempt"], 2);
    }

    #[tokio::test]
    async fn a_check_that_does_not_answer_soft_blocks() {
        let (slow, _a) = serve("200 OK", r#"{"status":"OK"}"#, 1500).await;
        let (broken, _b) = serve("500 Internal Server Error", "{}", 0).await;
        let (odd, _c) = serve("200 OK", r#"{"status":"MAYBE"}"#, 0).await;
        let (garbage, _d) = serve("200 OK", "not json", 0).await;
        let checks = vec![
            check("slow", &slow, Some("1s")),
            check("broken", &broken, None),
            check("odd", &odd, None),
            check("garbage", &garbage, None),
            check("nowhere", "http://127.0.0.1:1/check", None),
        ];
        let outcomes = ask_pre_checks(
            &unused_client(),
            &config(checks),
            Mode::Preview,
            &Ask::default(),
        )
        .await;
        assert_eq!(outcomes.len(), 5);
        for o in &outcomes {
            assert_eq!(o.status, PreStatus::SoftBlock, "{}", o.name);
            assert_eq!(
                o.title.as_deref(),
                Some(format!("check {} didn't respond", o.name).as_str())
            );
        }
        assert_eq!(outcomes[0].message.as_deref(), Some("no answer within 1s"));
        assert_eq!(
            outcomes[1].message.as_deref(),
            Some("answered HTTP 500 Internal Server Error")
        );
        assert_eq!(
            outcomes[2].message.as_deref(),
            Some("unknown status \"MAYBE\"")
        );
    }

    #[test]
    fn hard_blocks_and_unacknowledged_soft_blocks_hold() {
        let outcome = |name: &str, status| PreOutcome {
            name: name.into(),
            status,
            title: None,
            message: None,
            details_url: None,
            started_at: 0,
            finished_at: 0,
        };
        let outcomes = vec![
            outcome("ok", PreStatus::Ok),
            outcome("soft", PreStatus::SoftBlock),
            outcome("hard", PreStatus::HardBlock),
        ];
        let names =
            |held: Vec<&PreOutcome>| held.iter().map(|o| o.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(holding(&outcomes, &[])), vec!["soft", "hard"]);
        assert_eq!(
            names(holding(&outcomes, &["soft".into(), "hard".into()])),
            vec!["hard"]
        );
    }
}
