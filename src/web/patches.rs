//! Manifest patches on the deploy page: the list in the advanced form and
//! the add-patch flow that replaces the list in place (resource → knob →
//! value, with a custom form one link away).
//!
//! Nothing here writes to the cluster. Removing an existing patch or adding
//! a new one edits the pending [`PatchChanges`] carried in the page's query
//! string, and the deploy applies them. The flow still dry-runs a new patch
//! against Kubernetes before accepting it, so a patch that cannot apply is
//! refused with the API server's message while the person is looking.
//!
//! The easy path builds the JSON Patch for the person: "Replicas" on a
//! Deployment becomes `replace /spec/replicas`, an env var becomes an
//! `add` on `/…/env/-` or a `replace` of the existing entry's value. The
//! pointer is always shown so the easy path teaches the custom one.

use std::collections::HashMap;

use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use kube::{Client, ResourceExt};
use maud::{html, Markup};

use crate::kubernetes::api::get_deploy_config;
use crate::kubernetes::deploy_config::Template;
use crate::kubernetes::patches::{ManifestPatch, PatchChanges, PatchOp, PatchTarget};
use crate::kubernetes::selections::Durability;
use crate::kubernetes::DeployConfig;
use crate::web::formatting;
use crate::web::preview::render_durability_badge;
use crate::web::Action;

/// `file|kind|name`, the form's handle on one manifest template.
fn target_value(template: &Template) -> Option<String> {
    let kind = template.manifest.get("kind")?.as_str()?;
    let name = template.manifest.get("metadata")?.get("name")?.as_str()?;
    Some(format!(
        "{}|{}|{}",
        template.file.clone().unwrap_or_default(),
        kind,
        name
    ))
}

fn parse_target(value: &str) -> Result<PatchTarget, String> {
    let mut parts = value.splitn(3, '|');
    let file = parts.next().filter(|f| !f.is_empty()).map(String::from);
    let kind = parts.next().unwrap_or_default().to_string();
    let name = parts.next().unwrap_or_default().to_string();
    if kind.is_empty() || name.is_empty() {
        return Err("Pick a manifest to patch".to_string());
    }
    Ok(PatchTarget { file, kind, name })
}

fn find_template(config: &DeployConfig, target: &PatchTarget) -> Option<Template> {
    config.resource_templates().into_iter().find(|t| {
        let kind = t.manifest.get("kind").and_then(|k| k.as_str());
        let name = t
            .manifest
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str());
        kind == Some(target.kind.as_str())
            && name == Some(target.name.as_str())
            && target
                .file
                .as_deref()
                .is_none_or(|f| t.file.as_deref() == Some(f))
    })
}

/// The knobs the easy flow offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knob {
    Replicas,
    Env,
    Resources,
    Image,
    Args,
}

impl Knob {
    pub fn id(self) -> &'static str {
        match self {
            Knob::Replicas => "replicas",
            Knob::Env => "env",
            Knob::Resources => "resources",
            Knob::Image => "image",
            Knob::Args => "args",
        }
    }

    pub fn parse(id: &str) -> Option<Knob> {
        match id {
            "replicas" => Some(Knob::Replicas),
            "env" => Some(Knob::Env),
            "resources" => Some(Knob::Resources),
            "image" => Some(Knob::Image),
            "args" => Some(Knob::Args),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Knob::Replicas => "Replicas",
            Knob::Env => "Env var on a container",
            Knob::Resources => "Requests and limits",
            Knob::Image => "Image",
            Knob::Args => "Args",
        }
    }

    /// The pointer the knob patches, abbreviated for the pick list.
    pub fn pointer_hint(self) -> &'static str {
        match self {
            Knob::Replicas => "/spec/replicas",
            Knob::Env => "/…/env",
            Knob::Resources => "/…/resources",
            Knob::Image => "/…/image",
            Knob::Args => "/…/args",
        }
    }

    fn needs_container(self) -> bool {
        self != Knob::Replicas
    }
}

/// Where a kind keeps its pod spec.
pub fn pod_spec_pointer(kind: &str) -> Option<&'static str> {
    match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" => {
            Some("/spec/template/spec")
        }
        "CronJob" => Some("/spec/jobTemplate/spec/template/spec"),
        "Pod" => Some("/spec"),
        _ => None,
    }
}

/// The knobs that make sense for a manifest, by kind.
pub fn knobs_for(manifest: &serde_json::Value) -> Vec<Knob> {
    let kind = manifest.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let mut knobs = Vec::new();
    if matches!(kind, "Deployment" | "StatefulSet" | "ReplicaSet") {
        knobs.push(Knob::Replicas);
    }
    if pod_spec_pointer(kind).is_some() {
        knobs.extend([Knob::Env, Knob::Resources, Knob::Image, Knob::Args]);
    }
    knobs
}

fn pointer_get<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a serde_json::Value> {
    value.pointer(pointer)
}

/// The containers of a manifest's pod spec: `(index, name)`.
pub fn containers(manifest: &serde_json::Value) -> Vec<(usize, String)> {
    let kind = manifest.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let Some(pod) = pod_spec_pointer(kind) else {
        return vec![];
    };
    pointer_get(manifest, &format!("{pod}/containers"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, c)| {
                    (
                        i,
                        c.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("container")
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
    form.get(key).map(|s| s.trim()).unwrap_or("")
}

/// Build the patch a knob describes from the form's fields. The manifest
/// decides between `add` and `replace`: replacing something that is not
/// there is refused by JSON Patch, so a missing `resources` or `args` is
/// added instead.
pub fn build_knob_patch(
    manifest: &serde_json::Value,
    target: PatchTarget,
    knob: Knob,
    form: &HashMap<String, String>,
    durability: Durability,
) -> Result<ManifestPatch, String> {
    let kind = target.kind.clone();
    let container_index: usize = field(form, "container").parse().unwrap_or(0);
    let base = if knob.needs_container() {
        let pod =
            pod_spec_pointer(&kind).ok_or_else(|| format!("{kind} has no pod spec to patch"))?;
        let path = format!("{pod}/containers/{container_index}");
        if pointer_get(manifest, &path).is_none() {
            return Err(format!(
                "{kind}/{} has no container {container_index}",
                target.name
            ));
        }
        path
    } else {
        String::new()
    };
    let op_for = |path: &str| {
        if pointer_get(manifest, path).is_some() {
            PatchOp::Replace
        } else {
            PatchOp::Add
        }
    };
    let (op, path, value) = match knob {
        Knob::Replicas => {
            let raw = field(form, "value");
            let replicas: i64 = raw
                .parse()
                .map_err(|_| format!("Replicas must be a whole number, not '{raw}'"))?;
            if replicas < 0 {
                return Err("Replicas cannot be negative".to_string());
            }
            let path = "/spec/replicas".to_string();
            (op_for(&path), path, Some(serde_json::json!(replicas)))
        }
        Knob::Env => {
            let name = field(form, "env_name");
            if name.is_empty() {
                return Err("The env var needs a name".to_string());
            }
            let value = field(form, "env_value").to_string();
            let env_path = format!("{base}/env");
            let existing = pointer_get(manifest, &env_path)
                .and_then(|e| e.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .position(|e| e.get("name").and_then(|n| n.as_str()) == Some(name))
                });
            match (pointer_get(manifest, &env_path), existing) {
                (_, Some(index)) => (
                    PatchOp::Replace,
                    format!("{env_path}/{index}/value"),
                    Some(serde_json::json!(value)),
                ),
                (Some(_), None) => (
                    PatchOp::Add,
                    format!("{env_path}/-"),
                    Some(serde_json::json!({ "name": name, "value": value })),
                ),
                (None, None) => (
                    PatchOp::Add,
                    env_path,
                    Some(serde_json::json!([{ "name": name, "value": value }])),
                ),
            }
        }
        Knob::Image => {
            let image = field(form, "image");
            if image.is_empty() {
                return Err("The image needs a value".to_string());
            }
            let path = format!("{base}/image");
            (op_for(&path), path, Some(serde_json::json!(image)))
        }
        Knob::Args => {
            let raw = field(form, "args");
            let args: Vec<String> = if raw.starts_with('[') {
                serde_json::from_str(raw).map_err(|e| format!("Args is not a JSON array: {e}"))?
            } else {
                raw.split_whitespace().map(String::from).collect()
            };
            let path = format!("{base}/args");
            (op_for(&path), path, Some(serde_json::json!(args)))
        }
        Knob::Resources => {
            let mut resources = serde_json::Map::new();
            for (section, keys) in [
                (
                    "requests",
                    [("cpu", "cpu_request"), ("memory", "memory_request")],
                ),
                ("limits", [("cpu", "cpu_limit"), ("memory", "memory_limit")]),
            ] {
                let mut entries = serde_json::Map::new();
                for (name, key) in keys {
                    let v = field(form, key);
                    if !v.is_empty() {
                        entries.insert(name.to_string(), serde_json::json!(v));
                    }
                }
                if !entries.is_empty() {
                    resources.insert(section.to_string(), serde_json::Value::Object(entries));
                }
            }
            if resources.is_empty() {
                return Err("Fill in at least one request or limit".to_string());
            }
            let path = format!("{base}/resources");
            (
                op_for(&path),
                path,
                Some(serde_json::Value::Object(resources)),
            )
        }
    };
    Ok(ManifestPatch {
        target,
        op,
        path,
        value,
        durability,
        note: None,
        by: None,
        since: Some(chrono::Utc::now().to_rfc3339()),
    })
}

/// The page URL for `action` on `config`.
fn page_url(config: &DeployConfig, action: &Action) -> String {
    format!(
        "/deploy?selected={}&{}",
        encode(&config.name_any()),
        action.as_params()
    )
}

/// The current patches and the pending edits: a kept patch offers Remove,
/// a removed one Keep, an addition Remove; each is a link to the same page
/// with the edits changed. "Add patch" opens the flow in place.
pub fn render_patch_list(config: &DeployConfig, action: &Action) -> Markup {
    let name = config.name_any();
    let pending = action.patch_changes().cloned().unwrap_or_default();
    let durability = action.durability().unwrap_or(Durability::Temporary);
    let with = |changes: PatchChanges| page_url(config, &action.with_patch_changes(changes));
    let flow_url = format!(
        "/fragments/patch-flow/{}?step=resource&return_url={}",
        name,
        encode(&page_url(config, action))
    );
    html! {
        @if config.spec.spec.patches.is_empty() && pending.add.is_empty() {
            p.muted { "None" }
        }
        @for (i, p) in config.spec.spec.patches.iter().enumerate() {
            @let removed = pending.removes(i);
            div.patch-item.patch-item--removed[removed] {
                div.patch-item__row {
                    code.patch-item__op { (p.describe_op()) }
                    @if removed {
                        a.link-button href=(with(pending.keeping(i))) { "Keep" }
                    } @else {
                        a.link-button href=(with(pending.removing(i))) { "Remove" }
                    }
                }
                div.patch-item__meta {
                    @if removed {
                        span.patch-item__removed { "removed in this deploy" }
                    } @else {
                        (render_durability_badge(p.durability))
                        @if let Some(note) = &p.note { span { " · \u{201c}" (note) "\u{201d}" } }
                        @if let Some(since) = p.since.as_deref().and_then(formatting::rfc3339_to_ms) {
                            span { " · " (formatting::format_ago_short(since)) }
                        }
                    }
                }
            }
        }
        @for (j, p) in pending.add.iter().enumerate() {
            div.patch-item {
                div.patch-item__row {
                    code.patch-item__op { (p.describe_op()) }
                    a.link-button href=(with(pending.without_addition(j))) { "Remove" }
                }
                div.patch-item__meta {
                    (render_durability_badge(durability))
                    span { " · added in this deploy" }
                }
            }
        }
        div.patch-add {
            a href="#patch-flow" hx-get=(flow_url) hx-target="#patch-flow" hx-swap="innerHTML" { "Add patch" }
        }
        div id="patch-flow" {}
    }
}

fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Everything a step of the flow needs to render itself and link onward.
/// `return_url` is the page the flow came from; its query string carries
/// the action, and with it the durability and the pending edits.
struct Flow<'a> {
    config: &'a DeployConfig,
    return_url: String,
}

impl Flow<'_> {
    fn durability(&self) -> Durability {
        action_of(&self.return_url)
            .durability()
            .unwrap_or(Durability::Temporary)
    }

    fn step_url(&self, step: &str, extra: &[(&str, &str)]) -> String {
        let mut url = format!(
            "/fragments/patch-flow/{}?step={}&return_url={}",
            self.config.name_any(),
            step,
            encode(&self.return_url)
        );
        for (k, v) in extra {
            url.push_str(&format!("&{k}={}", encode(v)));
        }
        url
    }

    fn eyebrow(&self, text: &str) -> Markup {
        html! { div.patch-flow__eyebrow { (text) } }
    }

    /// The bordered radio rows the resource and knob steps share. Picking a
    /// row loads the next step in place.
    fn pick_row(&self, url: &str, selected: bool, body: Markup) -> Markup {
        html! {
            label.pick-row.pick-row--selected[selected] {
                input type="radio" name="pick" checked[selected] hx-get=(url) hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                (body)
            }
        }
    }

    fn render_resource_step(&self) -> Markup {
        let templates = self.config.resource_templates();
        html! {
            div.patch-flow {
                (self.eyebrow("Step 1 · resource"))
                div.pick-list {
                    @for template in &templates {
                        @if let Some(value) = target_value(template) {
                            @let kind = template.manifest.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                            @let name = template.manifest.get("metadata").and_then(|m| m.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                            (self.pick_row(&self.step_url("knob", &[("target", &value)]), false, html! {
                                span { strong { (kind) } " " (name) @if let Some(f) = &template.file { span.faint { " · " (f) } } }
                            }))
                        }
                    }
                    @if templates.is_empty() {
                        p.muted { "No manifests to patch." }
                    }
                }
                div.patch-flow__footer {
                    a href="#patch-flow" hx-get=(self.step_url("closed", &[])) hx-target="#patch-flow" hx-swap="innerHTML" { "Cancel" }
                }
            }
        }
    }

    fn render_target_line(&self, target: &PatchTarget, knob: Option<Knob>) -> Markup {
        html! {
            div.patch-flow__target {
                strong { (target.kind) } " " (target.name)
                @if let Some(knob) = knob { " · " (knob.label()) }
                a.patch-flow__change href="#patch-flow" hx-get=(self.step_url("resource", &[])) hx-target="#patch-flow" hx-swap="innerHTML" { "change" }
            }
        }
    }

    fn render_knob_step(
        &self,
        target: &PatchTarget,
        target_value: &str,
        template: &Template,
    ) -> Markup {
        let knobs = knobs_for(&template.manifest);
        html! {
            div.patch-flow {
                (self.eyebrow("Step 2 · knob"))
                (self.render_target_line(target, None))
                div.pick-list {
                    @for knob in &knobs {
                        (self.pick_row(&self.step_url("value", &[("target", target_value), ("knob", knob.id())]), false, html! {
                            span.pick-row__label { (knob.label()) }
                            code.pick-row__pointer { (knob.pointer_hint()) }
                        }))
                    }
                    @if knobs.is_empty() {
                        p.muted { "No knobs for a " (target.kind) "; write a custom patch." }
                    }
                }
                div.patch-flow__footer {
                    a href="#patch-flow" hx-get=(self.step_url("custom", &[("target", target_value)])) hx-target="#patch-flow" hx-swap="innerHTML" { "Custom patch" }
                }
            }
        }
    }

    /// Step 3. `form` carries what was typed so far (for the live "will be
    /// saved as" line and for re-rendering after a refusal).
    fn render_value_step(
        &self,
        target: &PatchTarget,
        target_value: &str,
        template: &Template,
        knob: Knob,
        form: &HashMap<String, String>,
        error: Option<&str>,
    ) -> Markup {
        let manifest = &template.manifest;
        let containers = containers(manifest);
        let container_index: usize = field(form, "container").parse().unwrap_or(0);
        let pod = pod_spec_pointer(&target.kind).unwrap_or("/spec");
        let base = format!("{pod}/containers/{container_index}");
        let current = |pointer: &str| {
            pointer_get(manifest, pointer).map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        };
        let typed_or = |key: &str, fallback: Option<String>| {
            if form.contains_key(key) {
                field(form, key).to_string()
            } else {
                fallback.unwrap_or_default()
            }
        };
        let preview = build_knob_patch(manifest, target.clone(), knob, form, self.durability())
            .map(|p| p.describe_op());
        let refresh_url = self.step_url("value", &[("target", target_value), ("knob", knob.id())]);
        let name = self.config.name_any();
        let err_class = if error.is_some() { "input--error" } else { "" };
        html! {
            div.patch-flow {
                (self.eyebrow("Step 3 · value"))
                (self.render_target_line(target, Some(knob)))
                form.patch-flow__form hx-post=(format!("/fragments/patch-flow/{name}/add")) hx-target="#patch-flow" hx-swap="innerHTML" {
                    input type="hidden" name="return_url" value=(self.return_url);
                    input type="hidden" name="flow" value="knob";
                    input type="hidden" name="knob" value=(knob.id());
                    input type="hidden" name="target" value=(target_value);
                    @if knob.needs_container() && containers.len() > 1 {
                        label.patch-flow__label { "Container" }
                        select name="container" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change" {
                            @for (i, cname) in &containers {
                                option value=(i) selected[*i == container_index] { (cname) }
                            }
                        }
                    } @else if knob.needs_container() {
                        input type="hidden" name="container" value=(container_index);
                    }
                    @match knob {
                        Knob::Replicas => {
                            label.patch-flow__label { "Replicas" }
                            div.patch-flow__inline {
                                input class=(format!("mono short {err_class}")) type="text" name="value" value=(typed_or("value", current("/spec/replicas"))) hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                                @if let Some(now) = current("/spec/replicas") { span.faint { "manifest says " (now) } }
                            }
                        }
                        Knob::Env => {
                            label.patch-flow__label { "Name" }
                            input class=(format!("mono {err_class}")) type="text" name="env_name" value=(typed_or("env_name", None)) placeholder="FLAG" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                            label.patch-flow__label { "Value" }
                            input class=(format!("mono {err_class}")) type="text" name="env_value" value=(typed_or("env_value", None)) hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                        }
                        Knob::Image => {
                            label.patch-flow__label { "Image" }
                            input class=(format!("mono {err_class}")) type="text" name="image" value=(typed_or("image", current(&format!("{base}/image")))) hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                            @if let Some(now) = current(&format!("{base}/image")) { div.faint { "manifest says " code { (now) } } }
                        }
                        Knob::Args => {
                            label.patch-flow__label { "Args" }
                            input class=(format!("mono {err_class}")) type="text" name="args" value=(typed_or("args", current(&format!("{base}/args")))) placeholder="[\"--verbose\"] or space separated" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                        }
                        Knob::Resources => {
                            @let res = |section: &str, kind: &str| current(&format!("{base}/resources/{section}/{kind}"));
                            div.patch-flow__grid {
                                label.patch-flow__label { "CPU request" }
                                input class=(format!("mono {err_class}")) type="text" name="cpu_request" value=(typed_or("cpu_request", res("requests", "cpu"))) placeholder="100m" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                                label.patch-flow__label { "Memory request" }
                                input class=(format!("mono {err_class}")) type="text" name="memory_request" value=(typed_or("memory_request", res("requests", "memory"))) placeholder="128Mi" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                                label.patch-flow__label { "CPU limit" }
                                input class=(format!("mono {err_class}")) type="text" name="cpu_limit" value=(typed_or("cpu_limit", res("limits", "cpu"))) placeholder="500m" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                                label.patch-flow__label { "Memory limit" }
                                input class=(format!("mono {err_class}")) type="text" name="memory_limit" value=(typed_or("memory_limit", res("limits", "memory"))) placeholder="512Mi" hx-get=(refresh_url) hx-include="closest form" hx-target="#patch-flow" hx-swap="innerHTML" hx-trigger="change";
                            }
                        }
                    }
                    div.patch-preview {
                        div.patch-preview__label { "Will be saved as" }
                        @match &preview {
                            Ok(text) => { code { (text) } }
                            Err(why) => { span.muted { (why) } }
                        }
                    }
                    @if let Some(error) = error {
                        (render_refusal(error))
                    }
                    button.primary-button.block type="submit" { "Add patch" }
                }
            }
        }
    }

    fn render_custom_step(
        &self,
        target: &PatchTarget,
        target_value: &str,
        form: &HashMap<String, String>,
        error: Option<&str>,
    ) -> Markup {
        let name = self.config.name_any();
        let err_class = if error.is_some() { "input--error" } else { "" };
        let op = field(form, "op");
        html! {
            div.patch-flow {
                (self.eyebrow("Custom patch"))
                (self.render_target_line(target, None))
                form.patch-flow__form hx-post=(format!("/fragments/patch-flow/{name}/add")) hx-target="#patch-flow" hx-swap="innerHTML" {
                    input type="hidden" name="return_url" value=(self.return_url);
                    input type="hidden" name="flow" value="custom";
                    input type="hidden" name="target" value=(target_value);
                    label.patch-flow__label { "Op" }
                    select name="op" {
                        option value="add" selected[op == "add"] { "add" }
                        option value="replace" selected[op == "replace" || op.is_empty()] { "replace" }
                        option value="remove" selected[op == "remove"] { "remove" }
                    }
                    label.patch-flow__label { "Path" }
                    input class=(format!("mono {err_class}")) type="text" name="path" value=(field(form, "path")) placeholder="/spec/template/spec/containers/0/env/-" required;
                    label.patch-flow__label { "Value" }
                    input class=(format!("mono {err_class}")) type="text" name="value" value=(field(form, "value")) placeholder="{\"name\":\"FLAG\",\"value\":\"on\"}";
                    div.patch-flow__hint {
                        a href="https://datatracker.ietf.org/doc/html/rfc6902" target="_blank" rel="noopener noreferrer" { "Patch reference" }
                        " · JSON if it parses, otherwise text. End a path with "
                        code { "/-" }
                        " to append to a list."
                    }
                    @if let Some(error) = error {
                        (render_refusal(error))
                    }
                    button.primary-button.block type="submit" { "Add patch" }
                }
            }
        }
    }
}

fn render_refusal(message: &str) -> Markup {
    html! {
        div.inline-error {
            strong { "Refused" }
            code { (message) }
        }
    }
}

fn return_url_of(form: &HashMap<String, String>, name: &str) -> String {
    match form.get("return_url") {
        Some(url) if url.starts_with('/') && !url.starts_with("//") => url.clone(),
        _ => format!("/deploy?selected={name}&action=deploy-advanced"),
    }
}

/// The action a page URL describes, from its query string.
fn action_of(page_url: &str) -> Action {
    let query: HashMap<String, String> = page_url
        .split_once('?')
        .map(|(_, q)| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    Action::from_query(&query)
}

/// Render the step a query names. `error` re-renders the value or custom
/// step after Kubernetes or the form refused the patch.
fn render_step(
    config: &DeployConfig,
    query: &HashMap<String, String>,
    error: Option<&str>,
) -> Markup {
    let flow = Flow {
        config,
        return_url: return_url_of(query, &config.name_any()),
    };
    let step = field(query, "step");
    if step == "closed"
        || step == "resource" && error.is_none() && field(query, "target").is_empty()
    {
        return if step == "closed" {
            html! {}
        } else {
            flow.render_resource_step()
        };
    }
    let target_value = field(query, "target").to_string();
    let target = match parse_target(&target_value) {
        Ok(t) => t,
        Err(_) => return flow.render_resource_step(),
    };
    let Some(template) = find_template(config, &target) else {
        return html! {
            div.patch-flow {
                p.muted { "The " (target.kind) " " (target.name) " manifest is no longer part of this config." }
                div.patch-flow__footer {
                    a href="#patch-flow" hx-get=(flow.step_url("resource", &[])) hx-target="#patch-flow" hx-swap="innerHTML" { "Pick another" }
                }
            }
        };
    };
    match step {
        "custom" => flow.render_custom_step(&target, &target_value, query, error),
        "value" => match Knob::parse(field(query, "knob")) {
            Some(knob) => {
                flow.render_value_step(&target, &target_value, &template, knob, query, error)
            }
            None => flow.render_knob_step(&target, &target_value, &template),
        },
        _ => flow.render_knob_step(&target, &target_value, &template),
    }
}

#[get("/fragments/patch-flow/{name}")]
pub async fn patch_flow(
    path: web::Path<String>,
    query: web::Query<HashMap<String, String>>,
    client: web::Data<Client>,
) -> impl Responder {
    let name = path.into_inner();
    let config = match get_deploy_config(&client, &name).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."))
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}: {}", name, e);
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."));
        }
    };
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_step(&config, &query, None).into_string())
}

/// A value typed into the form: JSON if it parses, otherwise the raw string.
fn parse_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

fn patch_from_form(form: &HashMap<String, String>) -> Result<ManifestPatch, String> {
    let target = parse_target(field(form, "target"))?;
    let op = match form.get("op").map(String::as_str) {
        Some("add") => PatchOp::Add,
        Some("remove") => PatchOp::Remove,
        _ => PatchOp::Replace,
    };
    let path = field(form, "path").to_string();
    if !path.starts_with('/') {
        return Err("The path must be a JSON pointer starting with /".to_string());
    }
    let value = form
        .get("value")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(parse_value);
    let clean = |k: &str| {
        form.get(k)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok(ManifestPatch {
        target,
        op,
        path,
        value,
        durability: Durability::Temporary,
        note: clean("note"),
        by: clean("by"),
        since: None,
    })
}

/// The patch a form describes: built from a knob when the form came from
/// the easy flow, otherwise read as a custom patch.
fn patch_from_request(
    config: &DeployConfig,
    form: &HashMap<String, String>,
) -> Result<ManifestPatch, String> {
    if field(form, "flow") != "knob" {
        return patch_from_form(form);
    }
    let target = parse_target(field(form, "target"))?;
    let knob = Knob::parse(field(form, "knob")).ok_or_else(|| "Pick a knob".to_string())?;
    let template = find_template(config, &target)
        .ok_or_else(|| format!("{}/{} is not part of this config", target.kind, target.name))?;
    build_knob_patch(
        &template.manifest,
        target,
        knob,
        form,
        Durability::Temporary,
    )
}

fn is_htmx(req: &HttpRequest) -> bool {
    req.headers().contains_key("HX-Request")
}

/// Refuse with the step re-rendered around the message when the request
/// came from the flow, or as plain text otherwise.
fn refuse(
    req: &HttpRequest,
    config: &DeployConfig,
    form: &HashMap<String, String>,
    message: &str,
) -> HttpResponse {
    if !is_htmx(req) {
        return HttpResponse::BadRequest().body(message.to_string());
    }
    let mut query = form.clone();
    query.insert(
        "step".to_string(),
        if field(form, "flow") == "knob" {
            "value".to_string()
        } else {
            "custom".to_string()
        },
    );
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_step(config, &query, Some(message)).into_string())
}

/// The flow's last step: build the patch, check it against the cluster on
/// top of everything else the deploy would change, and send the person
/// back to the page with the patch pending. Nothing is written.
#[post("/fragments/patch-flow/{name}/add")]
pub async fn patch_flow_add(
    req: HttpRequest,
    path: web::Path<String>,
    client: web::Data<Client>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let name = path.into_inner();
    let config = match get_deploy_config(&client, &name).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."))
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}: {}", name, e);
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."));
        }
    };
    let patch = match patch_from_request(&config, &form) {
        Ok(p) => p,
        Err(message) => return refuse(&req, &config, &form, &message),
    };
    let return_url = return_url_of(&form, &name);
    let action = action_of(&return_url);
    let mut changes = action.patch_changes().cloned().unwrap_or_default();
    changes.add.push(patch);
    let action = match action {
        Action::DeployAdvanced { .. } => action.with_patch_changes(changes),
        _ => Action::DeployAdvanced {
            choices: Default::default(),
            durability: Durability::Temporary,
            patches: changes,
        },
    };
    let effective = action.effective_config(&config);
    if let Err(e) = crate::deploys::validate_patches(&effective, &client).await {
        return refuse(&req, &config, &form, &e.to_string());
    }
    let next = page_url(&config, &action);
    if is_htmx(&req) {
        HttpResponse::NoContent()
            .append_header(("HX-Redirect", next))
            .finish()
    } else {
        HttpResponse::SeeOther()
            .append_header(("Location", next))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deployment() -> serde_json::Value {
        json!({"kind": "Deployment", "metadata": {"name": "web"},
        "spec": {"replicas": 2, "template": {"spec": {"containers": [
            {"name": "app", "image": "ghcr.io/x/app:commit-$SHA", "env": [{"name": "LOG", "value": "info"}]},
            {"name": "sidecar", "image": "nginx:1.27.4"}
        ]}}}})
    }

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn target() -> PatchTarget {
        PatchTarget {
            file: None,
            kind: "Deployment".into(),
            name: "web".into(),
        }
    }

    #[test]
    fn form_values_parse_as_json_when_possible() {
        assert_eq!(parse_value("3"), json!(3));
        assert_eq!(parse_value(r#"{"a":1}"#), json!({"a": 1}));
        assert_eq!(parse_value("debug"), json!("debug"));
    }

    #[test]
    fn form_to_patch_validates_target_and_path() {
        let mut f = form(&[
            ("target", "deployment.yaml|Deployment|web"),
            ("op", "replace"),
            ("path", "/spec/replicas"),
            ("value", "5"),
        ]);
        let p = patch_from_form(&f).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.target.file.as_deref(), Some("deployment.yaml"));
        assert_eq!(p.target.kind, "Deployment");
        assert_eq!(p.value, Some(json!(5)));
        assert_eq!(p.durability, Durability::Temporary);

        f.insert("path".to_string(), "spec/replicas".to_string());
        assert!(patch_from_form(&f).is_err());
        f.insert("path".to_string(), "/x".to_string());
        f.insert("target".to_string(), "||".to_string());
        assert!(patch_from_form(&f).is_err());
    }

    #[test]
    fn knobs_depend_on_kind() {
        assert_eq!(
            knobs_for(&deployment()),
            vec![
                Knob::Replicas,
                Knob::Env,
                Knob::Resources,
                Knob::Image,
                Knob::Args
            ]
        );
        let cron = json!({"kind": "CronJob"});
        assert_eq!(
            knobs_for(&cron),
            vec![Knob::Env, Knob::Resources, Knob::Image, Knob::Args]
        );
        assert!(knobs_for(&json!({"kind": "Service"})).is_empty());
        assert_eq!(
            containers(&deployment()),
            vec![(0, "app".to_string()), (1, "sidecar".to_string())]
        );
        assert_eq!(Knob::parse("env"), Some(Knob::Env));
        assert_eq!(Knob::parse("nope"), None);
    }

    #[test]
    fn replicas_knob_replaces_or_adds() {
        let p = build_knob_patch(
            &deployment(),
            target(),
            Knob::Replicas,
            &form(&[("value", "3")]),
            Durability::Temporary,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.op, PatchOp::Replace);
        assert_eq!(p.path, "/spec/replicas");
        assert_eq!(p.value, Some(json!(3)));
        assert_eq!(
            p.describe_op(),
            "replace /spec/replicas = 3 on Deployment/web"
        );

        let bare = json!({"kind": "Deployment", "metadata": {"name": "web"}, "spec": {}});
        let p = build_knob_patch(
            &bare,
            target(),
            Knob::Replicas,
            &form(&[("value", "1")]),
            Durability::Standing,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.op, PatchOp::Add);
        assert!(build_knob_patch(
            &deployment(),
            target(),
            Knob::Replicas,
            &form(&[("value", "three")]),
            Durability::Standing
        )
        .is_err());
    }

    #[test]
    fn env_knob_replaces_an_existing_var_and_appends_a_new_one() {
        let existing = build_knob_patch(
            &deployment(),
            target(),
            Knob::Env,
            &form(&[
                ("container", "0"),
                ("env_name", "LOG"),
                ("env_value", "debug"),
            ]),
            Durability::Temporary,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(existing.op, PatchOp::Replace);
        assert_eq!(
            existing.path,
            "/spec/template/spec/containers/0/env/0/value"
        );
        assert_eq!(existing.value, Some(json!("debug")));

        let new = build_knob_patch(
            &deployment(),
            target(),
            Knob::Env,
            &form(&[
                ("container", "0"),
                ("env_name", "FLAG"),
                ("env_value", "on"),
            ]),
            Durability::Temporary,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(new.op, PatchOp::Add);
        assert_eq!(new.path, "/spec/template/spec/containers/0/env/-");
        assert_eq!(new.value, Some(json!({"name": "FLAG", "value": "on"})));

        // A container with no env array gets one.
        let sidecar = build_knob_patch(
            &deployment(),
            target(),
            Knob::Env,
            &form(&[
                ("container", "1"),
                ("env_name", "FLAG"),
                ("env_value", "on"),
            ]),
            Durability::Temporary,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(sidecar.op, PatchOp::Add);
        assert_eq!(sidecar.path, "/spec/template/spec/containers/1/env");
        assert!(build_knob_patch(
            &deployment(),
            target(),
            Knob::Env,
            &form(&[("container", "5"), ("env_name", "X")]),
            Durability::Temporary
        )
        .is_err());
        assert!(build_knob_patch(
            &deployment(),
            target(),
            Knob::Env,
            &form(&[("env_name", "")]),
            Durability::Temporary
        )
        .is_err());
    }

    #[test]
    fn image_args_and_resources_knobs() {
        let image = build_knob_patch(
            &deployment(),
            target(),
            Knob::Image,
            &form(&[("container", "1"), ("image", "nginx:1.27.5")]),
            Durability::Standing,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(image.path, "/spec/template/spec/containers/1/image");
        assert_eq!(image.op, PatchOp::Replace);

        let args = build_knob_patch(
            &deployment(),
            target(),
            Knob::Args,
            &form(&[("args", "--verbose --port 80")]),
            Durability::Standing,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(args.op, PatchOp::Add, "no args in the manifest yet");
        assert_eq!(args.value, Some(json!(["--verbose", "--port", "80"])));
        let json_args = build_knob_patch(
            &deployment(),
            target(),
            Knob::Args,
            &form(&[("args", r#"["a", "b c"]"#)]),
            Durability::Standing,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(json_args.value, Some(json!(["a", "b c"])));

        let resources = build_knob_patch(
            &deployment(),
            target(),
            Knob::Resources,
            &form(&[("cpu_request", "100m"), ("memory_limit", "512Mi")]),
            Durability::Standing,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(resources.path, "/spec/template/spec/containers/0/resources");
        assert_eq!(
            resources.value,
            Some(json!({"requests": {"cpu": "100m"}, "limits": {"memory": "512Mi"}}))
        );
        assert!(build_knob_patch(
            &deployment(),
            target(),
            Knob::Resources,
            &form(&[]),
            Durability::Standing
        )
        .is_err());
    }

    #[test]
    fn cronjobs_patch_the_job_template() {
        let cron = json!({"kind": "CronJob", "metadata": {"name": "tick"},
            "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "job"}]}}}}}});
        let p = build_knob_patch(
            &cron,
            PatchTarget {
                file: None,
                kind: "CronJob".into(),
                name: "tick".into(),
            },
            Knob::Image,
            &form(&[("image", "x:1")]),
            Durability::Standing,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            p.path,
            "/spec/jobTemplate/spec/template/spec/containers/0/image"
        );
        assert_eq!(p.op, PatchOp::Add);
    }

    #[test]
    fn the_page_url_carries_the_action() {
        let action = Action::DeployAdvanced {
            choices: Default::default(),
            durability: Durability::Standing,
            patches: PatchChanges {
                remove: vec![0],
                add: vec![],
            },
        };
        let config = DeployConfig::new(
            "web",
            crate::kubernetes::deploy_config::DeployConfigSpec {
                spec: crate::kubernetes::deploy_config::DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters: Default::default(),
                    selections: Default::default(),
                    patches: vec![],
                    config: crate::kubernetes::repo::Repository {
                        owner: "o".into(),
                        repo: "c".into(),
                    },
                    specs: vec![],
                },
            },
        );
        let url = page_url(&config, &action);
        assert!(url.starts_with("/deploy?selected=web&action=deploy-advanced"));
        assert_eq!(action_of(&url), action);
        assert_eq!(action_of("/deploy?selected=web"), Action::DeployLatest);
    }

    #[test]
    fn targets_round_trip() {
        let t = Template {
            file: Some("deployment.yaml".into()),
            manifest: deployment(),
        };
        let value = target_value(&t).unwrap_or_default();
        assert_eq!(value, "deployment.yaml|Deployment|web");
        let parsed = parse_target(&value).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed.file.as_deref(), Some("deployment.yaml"));
        assert!(parse_target("|Deployment|").is_err());
    }
}
