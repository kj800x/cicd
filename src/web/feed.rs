//! The revision feed: how a revision reads as one entry, how a quiet run of
//! autodeploys collapses into one line, and how entries group by day. The
//! history page and the home page's "Recent activity" both render from
//! here so a revision reads the same everywhere.

use maud::{html, Markup};

use crate::db::revision::Revision;
use crate::db::revision_diff::{self, Change};
use crate::kubernetes::selections::Durability;
use crate::web::formatting;
use crate::web::preview::render_durability_badge;

/// The actor autodeploy records on the revisions it makes.
pub const AUTODEPLOY_ACTOR: &str = "autodeploy";

/// Consecutive autodeploys closer together than this collapse into a burst.
const BURST_GAP_MS: i64 = 10 * 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Fail,
    Warn,
}

impl Tone {
    pub fn class_name(self) -> &'static str {
        match self {
            Tone::Plain => "",
            Tone::Fail => "feed-row--fail",
            Tone::Warn => "feed-row--warn",
        }
    }
}

/// One revision, read for a person: the verb, its colour, who did it and
/// why, and the changes worth a glance.
#[derive(Clone, Debug)]
pub struct Entry {
    pub rev: Revision,
    pub verb: &'static str,
    pub tone: Tone,
    /// For a rollback, the revision that was replayed.
    pub rollback_to: Option<i64>,
    /// The reason a person typed, if the revision carries one.
    pub reason: Option<String>,
    pub changes: Vec<Change>,
}

/// `rollback to revision 812` or `rollback to revision 812: why` as the
/// deploy path records it.
fn parse_rollback(reason: &str) -> Option<(i64, Option<String>)> {
    let rest = reason.strip_prefix("rollback to revision ")?;
    let (id, note) = match rest.split_once(':') {
        Some((id, note)) => (id, Some(note.trim().to_string()).filter(|n| !n.is_empty())),
        None => (rest, None),
    };
    id.trim().parse().ok().map(|id| (id, note))
}

impl Entry {
    pub fn from(rev: Revision, prev: Option<&Revision>) -> Self {
        let changes = revision_diff::diff(&rev, prev);
        let reason = rev.reason.clone().unwrap_or_default();
        let (verb, tone, rollback_to, shown_reason) = if rev.action == "undeploy" {
            ("Undeployed", Tone::Fail, None, Some(reason.clone()))
        } else if rev.action == "patch" {
            ("Patches changed", Tone::Plain, None, None)
        } else if let Some((to, note)) = parse_rollback(&reason) {
            ("Rolled back", Tone::Fail, Some(to), note)
        } else if reason.starts_with("ended temporary deployment") {
            ("Ended temporary deployment", Tone::Warn, None, None)
        } else {
            ("Deployed", Tone::Plain, None, Some(reason.clone()))
        };
        Entry {
            rev,
            verb,
            tone,
            rollback_to,
            reason: shown_reason.filter(|r| !r.is_empty()),
            changes,
        }
    }

    pub fn is_autodeploy(&self) -> bool {
        self.rev.actor == AUTODEPLOY_ACTOR
    }

    /// The parameter changes, in the history change syntax.
    fn parameter_lines(&self) -> Vec<String> {
        self.changes
            .iter()
            .filter(|c| matches!(c, Change::Parameter { .. }))
            .map(Change::describe)
            .collect()
    }

    /// The config move, if any.
    fn config_line(&self) -> Option<String> {
        self.changes
            .iter()
            .find(|c| matches!(c, Change::Config { .. }))
            .map(Change::describe)
    }

    /// `1 patch added`, `2 patches removed`, or both.
    fn patch_summary(&self) -> Option<String> {
        let added = self
            .changes
            .iter()
            .filter(|c| matches!(c, Change::PatchAdded(_)))
            .count();
        let removed = self
            .changes
            .iter()
            .filter(|c| matches!(c, Change::PatchRemoved(_)))
            .count();
        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!(
                "{added} patch{} added",
                if added == 1 { "" } else { "es" }
            ));
        }
        if removed > 0 {
            parts.push(format!(
                "{removed} patch{} removed",
                if removed == 1 { "" } else { "es" }
            ));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// The first change, for the one-line forms.
    pub fn first_change(&self) -> Option<String> {
        self.parameter_lines()
            .into_iter()
            .next()
            .or_else(|| self.changes.first().map(Change::describe))
    }

    /// The one sentence: verb, rollback target, actor, reason, badge.
    pub fn render_sentence(&self, revision_links: bool) -> Markup {
        html! {
            strong class=(match self.tone { Tone::Fail => "feed-verb feed-verb--fail", Tone::Warn => "feed-verb feed-verb--warn", Tone::Plain => "feed-verb" }) { (self.verb) }
            @if let Some(to) = self.rollback_to {
                " to revision "
                @if revision_links { a href=(format!("/revisions/{to}")) { "#" (to) } } @else { "#" (to) }
            }
            span.muted { " · " (self.rev.actor) }
            @if let Some(reason) = &self.reason {
                span.muted { " · \u{201c}" (reason) "\u{201d}" }
            }
            @if self.rev.temporary && self.rev.action != "undeploy" {
                " " (render_durability_badge(Durability::Temporary))
            }
        }
    }

    /// The changes line under the sentence: parameters first, then the
    /// config move and the patch count in the faint colour. "no change"
    /// is printed rather than hidden.
    pub fn render_changes(&self) -> Markup {
        let params = self.parameter_lines();
        let config = self.config_line();
        let patches = self.patch_summary();
        let other: Vec<String> = self
            .changes
            .iter()
            .filter(|c| matches!(c, Change::Undeployed | Change::Deployed))
            .map(Change::describe)
            .collect();
        html! {
            @if self.changes.is_empty() {
                span.faint { "no change" }
            }
            @for (i, line) in other.iter().chain(params.iter()).enumerate() {
                span.feed-change.feed-change--first[i == 0] { (line) }
            }
            @if let Some(patches) = patches {
                span.feed-change.faint { (patches) }
            }
            @if let Some(config) = config {
                span.feed-change.faint { (config) }
            }
        }
    }
}

/// A feed item: one entry, or a burst of autodeploys shown as one line.
#[derive(Clone, Debug)]
pub enum Item {
    One(Box<Entry>),
    Burst(Vec<Entry>),
}

impl Item {
    pub fn created_at(&self) -> i64 {
        match self {
            Item::One(e) => e.rev.created_at,
            Item::Burst(entries) => entries.first().map(|e| e.rev.created_at).unwrap_or(0),
        }
    }
}

/// Collapse runs of autodeploys. `entries` is newest first; a run needs at
/// least two entries, each within [`BURST_GAP_MS`] of the next, and any
/// human deploy breaks it.
pub fn group_bursts(entries: Vec<Entry>) -> Vec<Item> {
    let mut items = Vec::new();
    let mut run: Vec<Entry> = Vec::new();
    let flush = |run: &mut Vec<Entry>, items: &mut Vec<Item>| {
        if run.len() >= 2 {
            items.push(Item::Burst(std::mem::take(run)));
        } else {
            for e in run.drain(..) {
                items.push(Item::One(Box::new(e)));
            }
        }
    };
    for entry in entries {
        let continues = entry.is_autodeploy()
            && run
                .last()
                .is_none_or(|last| last.rev.created_at - entry.rev.created_at <= BURST_GAP_MS);
        if !continues {
            flush(&mut run, &mut items);
        }
        if entry.is_autodeploy() {
            run.push(entry);
        } else {
            items.push(Item::One(Box::new(entry)));
        }
    }
    flush(&mut run, &mut items);
    items
}

/// Items grouped by calendar day, newest day first, each with its label.
pub fn group_days(items: Vec<Item>) -> Vec<(String, String, Vec<Item>)> {
    let today = chrono::Utc::now()
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive();
    let mut groups: Vec<(chrono::NaiveDate, Vec<Item>)> = Vec::new();
    for item in items {
        let Some(day) = formatting::local_date(item.created_at()) else {
            continue;
        };
        match groups.last_mut() {
            Some((d, list)) if *d == day => list.push(item),
            _ => groups.push((day, vec![item])),
        }
    }
    groups
        .into_iter()
        .map(|(day, list)| {
            (
                formatting::day_label(day, today),
                day.format("%Y-%m-%d").to_string(),
                list,
            )
        })
        .collect()
}

fn burst_names(entries: &[Entry]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for e in entries {
        if !names.contains(&e.rev.config_name.as_str()) {
            names.push(&e.rev.config_name);
        }
    }
    names.join(", ")
}

fn burst_span(entries: &[Entry]) -> String {
    let first = entries.last().map(|e| e.rev.created_at).unwrap_or(0);
    let last = entries.first().map(|e| e.rev.created_at).unwrap_or(0);
    format!(
        "{} – {}",
        formatting::format_time_of_day(first),
        formatting::format_time_of_day(last)
    )
}

/// One full feed row: time · config · sentence and changes · Detail.
pub fn render_row(entry: &Entry) -> Markup {
    html! {
        div class=(format!("feed-row {}", entry.tone.class_name())) {
            span.feed-row__time { (formatting::format_time_of_day(entry.rev.created_at)) }
            a.feed-row__config href=(format!("/deploy-history/{}", entry.rev.config_name)) { (entry.rev.config_name) }
            div.feed-row__body {
                div.feed-row__sentence { (entry.render_sentence(true)) }
                div.feed-row__changes { (entry.render_changes()) }
                @if entry.rollback_to.is_some() {
                    div.feed-row__note { "Blocker added — deploys refused until cleared" }
                }
            }
            div.feed-row__actions {
                a href=(format!("/revisions/{}", entry.rev.id)) { "Detail" }
            }
        }
    }
}

/// A burst as a disclosure: count and names on the summary, one line per
/// autodeploy inside.
pub fn render_burst(entries: &[Entry]) -> Markup {
    html! {
        details.disclosure.feed-burst {
            summary.disclosure__summary {
                span.disclosure__caret {}
                span.disclosure__label { (entries.len()) " autodeploys · " (burst_names(entries)) }
                span.disclosure__meta { (burst_span(entries)) " · autodeploy" }
            }
            div.disclosure__panel.feed-burst__panel {
                @for e in entries {
                    div.feed-burst__line {
                        span.feed-row__time { (formatting::format_time_of_day(e.rev.created_at)) }
                        a.feed-burst__config href=(format!("/deploy-history/{}", e.rev.config_name)) { (e.rev.config_name) }
                        span.feed-burst__change { (e.first_change().unwrap_or_else(|| "no change".to_string())) }
                    }
                }
            }
        }
    }
}

/// The whole feed, grouped by day.
pub fn render_feed(items: Vec<Item>) -> Markup {
    let groups = group_days(items);
    html! {
        @for (label, date, items) in groups {
            div.feed-day {
                (label)
                span.feed-day__date { (date) }
            }
            @for item in &items {
                @match item {
                    Item::One(entry) => (render_row(entry)),
                    Item::Burst(entries) => (render_burst(entries)),
                }
            }
        }
    }
}

/// The compact form for the home page: time · config · sentence with the
/// first change inline. A burst is one muted line.
pub fn render_compact(items: &[Item]) -> Markup {
    html! {
        @for item in items {
            @match item {
                Item::One(entry) => {
                    div.feed-row.feed-row--compact {
                        span.feed-row__time { (formatting::format_time_of_day(entry.rev.created_at)) }
                        a.feed-row__config href=(format!("/deploy-history/{}", entry.rev.config_name)) { (entry.rev.config_name) }
                        span.feed-row__inline {
                            (entry.render_sentence(true))
                            @if let Some(change) = entry.first_change() {
                                " " span.feed-change { (change) }
                            }
                        }
                    }
                }
                Item::Burst(entries) => {
                    div.feed-row.feed-row--compact.feed-row--quiet {
                        span.feed-row__time { (formatting::format_time_of_day(entries.first().map(|e| e.rev.created_at).unwrap_or(0))) }
                        span.feed-row__config.muted { (burst_names(entries)) }
                        span.feed-row__inline.muted { (entries.len()) " autodeploys " span.faint { "· " (burst_span(entries)) } }
                    }
                }
            }
        }
    }
}

/// Entries for a newest-first list of revisions, each diffed against the
/// previous revision of the same config.
pub fn entries(revisions: Vec<Revision>) -> Vec<Entry> {
    Revision::with_previous(revisions)
        .into_iter()
        .map(|(rev, prev)| Entry::from(rev, prev.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::revision::RevisionParameter;

    fn rev(id: i64, actor: &str, action: &str, reason: Option<&str>, at: i64) -> Revision {
        Revision {
            id,
            config_name: "site".into(),
            created_at: at,
            actor: actor.into(),
            action: action.into(),
            reason: reason.map(String::from),
            config_sha: Some("c1".into()),
            config_branch: Some("master".into()),
            config_version_hash: None,
            patches: None,
            temporary: false,
            parameters: vec![RevisionParameter {
                name: "SHA".into(),
                kind: "commit".into(),
                value: format!("{id:0>8}"),
                branch: Some("master".into()),
            }],
        }
    }

    #[test]
    fn verbs_follow_the_action_and_the_reason() {
        let e = |action: &str, reason: Option<&str>| {
            Entry::from(rev(1, "web", action, reason, 0), None)
        };
        assert_eq!(e("deploy", None).verb, "Deployed");
        assert_eq!(e("undeploy", None).verb, "Undeployed");
        assert_eq!(e("patch", Some("added patch: x")).verb, "Patches changed");
        let rb = e("deploy", Some("rollback to revision 812: bad migration"));
        assert_eq!(rb.verb, "Rolled back");
        assert_eq!(rb.rollback_to, Some(812));
        assert_eq!(rb.reason.as_deref(), Some("bad migration"));
        assert_eq!(rb.tone, Tone::Fail);
        let plain_rb = e("deploy", Some("rollback to revision 7"));
        assert_eq!((plain_rb.rollback_to, plain_rb.reason), (Some(7), None));
        let ended = e("deploy", Some("ended temporary deployment: cleared SHA"));
        assert_eq!(ended.verb, "Ended temporary deployment");
        assert_eq!(ended.tone, Tone::Warn);
    }

    #[test]
    fn bursts_collapse_close_autodeploys_only() {
        let minute = 60_000;
        let list = vec![
            rev(6, "web", "deploy", None, 60 * minute),
            rev(5, "autodeploy", "deploy", None, 50 * minute),
            rev(4, "autodeploy", "deploy", None, 45 * minute),
            rev(3, "autodeploy", "deploy", None, 20 * minute),
            rev(2, "web", "deploy", None, 10 * minute),
            rev(1, "autodeploy", "deploy", None, 5 * minute),
        ];
        let items = group_bursts(entries(list));
        let shape: Vec<String> = items
            .iter()
            .map(|i| match i {
                Item::One(e) => format!("one:{}", e.rev.id),
                Item::Burst(es) => format!(
                    "burst:{}",
                    es.iter()
                        .map(|e| e.rev.id.to_string())
                        .collect::<Vec<_>>()
                        .join("+")
                ),
            })
            .collect();
        assert_eq!(
            shape,
            vec!["one:6", "burst:5+4", "one:3", "one:2", "one:1"],
            "a 25 minute gap and a human deploy both break a run"
        );
    }

    #[test]
    fn changes_line_prints_no_change_when_nothing_moved() {
        let a = rev(1, "web", "deploy", None, 0);
        let mut b = rev(2, "web", "deploy", None, 1);
        b.parameters = a.parameters.clone();
        let entry = Entry::from(b, Some(&a));
        assert!(entry.changes.is_empty());
        assert!(entry.render_changes().into_string().contains("no change"));
        assert_eq!(entry.first_change(), None);
    }
}
