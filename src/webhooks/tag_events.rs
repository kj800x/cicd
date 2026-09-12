//! Autodeploy for tag parameters, driven by watchtower's event feed.
//!
//! Every thirty seconds cicd reads the events after its cursor (kept in
//! sqlite, so nothing is lost while either side is down) and, for each
//! tag that appeared or moved, asks every config the same questions the
//! check-run autodeploy asks: flag on, not orphaned, a tag parameter on
//! that image that is tracking rather than pinned, the tag inside the
//! tracked range and newer than what is deployed, not a temporary
//! deployment, no blocker. Then it deploys latest, which resolves the
//! parameter through watchtower like any other deploy. A first run with
//! no cursor starts from "now": history is a baseline, not a backlog.

use std::time::Duration;

use kube::{Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::{
    crab_ext::Octocrabs,
    db::{blocker::Blocker, watchtower_cursor},
    deploys::{run_action, SelectionIntent},
    error::AppResult,
    kubernetes::{
        api::get_all_deploy_configs,
        parameters::ImageRef,
        selections::Mode,
        tags::{self, Candidate},
        DeployConfig,
    },
    watchtower::{Event, EventKind, Tag, Watchtower},
    web::Action,
};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const PAGE: usize = 200;

/// Why a config did not autodeploy for a tag event, for the log.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    Off,
    Orphaned,
    OtherImage,
    Pinned,
    OutsideRange(String),
    NotNewer(String),
    Temporary,
    Blocked,
}

/// The pure part: the name of the tag parameter that should move for
/// this tag of this image, or why not. `known` is the image's tags as
/// watchtower has them, which is where the deployed tag's rank comes from.
pub fn tag_parameter_for(
    config: &DeployConfig,
    image: &ImageRef,
    tag: &Candidate,
    known: &[Tag],
) -> Result<String, Skip> {
    if !config.autodeploy() {
        return Err(Skip::Off);
    }
    if config.is_orphaned() {
        return Err(Skip::Orphaned);
    }
    let deployed = config.parameter_values();
    let mut last = Skip::OtherImage;
    for (pname, source) in &config.spec.spec.parameters {
        let Some(source_image) = source.image_ref() else {
            continue;
        };
        if source_image != *image {
            continue;
        }
        let selection = config.selection(pname);
        let pattern = match selection.mode() {
            Mode::Pin(_) => {
                last = Skip::Pinned;
                continue;
            }
            Mode::Track(p) => p.to_string(),
            Mode::Default => source.default_channel().unwrap_or_default().to_string(),
        };
        if !tags::matches(tag, &pattern, source.variant()) {
            last = Skip::OutsideRange(pattern);
            continue;
        }
        let current = deployed
            .get(pname)
            .and_then(|value| known.iter().find(|t| t.tag == *value))
            .and_then(Candidate::of)
            .and_then(|c| tags::rank(&c));
        let newer = match current {
            Some(current) => tags::rank(tag).is_some_and(|new| new > current),
            None => true,
        };
        if !newer {
            last = Skip::NotNewer(deployed.get(pname).cloned().unwrap_or_default());
            continue;
        }
        if config.is_temporary_deployment() {
            return Err(Skip::Temporary);
        }
        return Ok(pname.clone());
    }
    Err(last)
}

pub struct TagEventPoller {
    pool: Pool<SqliteConnectionManager>,
    client: Client,
    octocrabs: Octocrabs,
}

impl TagEventPoller {
    pub fn new(pool: Pool<SqliteConnectionManager>, client: Client, octocrabs: Octocrabs) -> Self {
        Self {
            pool,
            client,
            octocrabs,
        }
    }

    /// Runs forever. Every failure is logged and the next tick retries;
    /// the cursor only advances past events that were handled.
    pub async fn run(self) {
        let watchtower = Watchtower::global();
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Err(e) = self.poll_once(watchtower).await {
                log::warn!("watchtower event poll failed: {}", e);
            }
        }
    }

    async fn poll_once(&self, watchtower: &Watchtower) -> AppResult<()> {
        let cursor = watchtower_cursor::get(&self.pool.get()?)?;
        let Some(after) = cursor else {
            let page = watchtower.events(0, 1).await?;
            watchtower_cursor::set(&self.pool.get()?, page.latest_id)?;
            log::info!(
                "watchtower events: starting from now (event {})",
                page.latest_id
            );
            return Ok(());
        };
        let page = watchtower.events(after, PAGE).await?;
        if page.events.is_empty() {
            return Ok(());
        }
        let configs = get_all_deploy_configs(&self.client).await?;
        for event in &page.events {
            self.handle(watchtower, event, &configs).await;
            watchtower_cursor::set(&self.pool.get()?, event.id)?;
        }
        Ok(())
    }

    async fn handle(&self, watchtower: &Watchtower, event: &Event, configs: &[DeployConfig]) {
        if event.kind == EventKind::Removed {
            return;
        }
        let Some(tag) = Candidate::of_event(event) else {
            log::debug!(
                "Tag autodeploy: {}:{} names no version, nothing tracks it",
                event.name,
                event.tag
            );
            return;
        };
        let image = ImageRef {
            registry: event.registry.clone(),
            name: event.name.clone(),
        };
        // The deployed tag's rank comes from the same reading of the image.
        let known = match watchtower.lookup(&image).await {
            Ok(Some(repo)) => repo.tag,
            Ok(None) => vec![],
            Err(e) => {
                log::warn!(
                    "Tag autodeploy: lookup of {}/{} failed, treating the deployed tag as unknown: {}",
                    image.registry,
                    image.name,
                    e
                );
                vec![]
            }
        };
        for config in configs {
            let name = config.name_any();
            let parameter = match tag_parameter_for(config, &image, &tag, &known) {
                Ok(p) => p,
                Err(Skip::Off) | Err(Skip::OtherImage) => continue,
                Err(skip) => {
                    log::info!(
                        "Tag autodeploy: skipping {} for {}:{} ({:?})",
                        name,
                        event.name,
                        event.tag,
                        skip
                    );
                    continue;
                }
            };
            let unblocked = self
                .pool
                .get()
                .ok()
                .and_then(|c| Blocker::active_for(&c, &name).ok())
                .is_some_and(|active| active.is_empty());
            if !unblocked {
                log::info!("Tag autodeploy: skipping {} ({:?})", name, Skip::Blocked);
                continue;
            }
            log::info!(
                "Tag autodeploy: {} tracks {} of {}; deploying after tag {} ({:?})",
                name,
                parameter,
                event.name,
                event.tag,
                event.kind
            );
            let result = run_action(
                &Action::DeployLatest,
                config,
                &self.client,
                &self.octocrabs,
                &self.pool,
                "autodeploy",
                &SelectionIntent::default(),
            )
            .await;
            crate::metrics::get().deploy_actions.add(
                1,
                &[
                    opentelemetry::KeyValue::new("name", name.clone()),
                    opentelemetry::KeyValue::new("action", "autodeploy"),
                    opentelemetry::KeyValue::new(
                        "result",
                        if result.is_ok() { "success" } else { "error" },
                    ),
                ],
            );
            if let Err(e) = result {
                log::error!("Tag autodeploy of {} failed: {}", name, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::deploy_config::{
        DeployConfigSpec, DeployConfigSpecFields, DeployConfigStatus,
    };
    use crate::kubernetes::parameters::{ParameterSource, ParameterValue};
    use crate::kubernetes::repo::ShaMaybeBranch;
    use crate::kubernetes::selections::{Durability, Selection};
    use crate::kubernetes::Repository;

    fn config(autodeploy: bool, deployed: &str) -> DeployConfig {
        let mut parameters = std::collections::BTreeMap::new();
        parameters.insert(
            "NGINX".to_string(),
            ParameterSource::Tag {
                image: "docker.io/library/nginx".into(),
                pattern: "1.27.*".into(),
                variant: None,
            },
        );
        let mut dc = DeployConfig::new(
            "site",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters,
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "o".into(),
                        repo: "c".into(),
                    },
                    specs: vec![],
                },
            },
        );
        let mut status_params = std::collections::BTreeMap::new();
        status_params.insert(
            "NGINX".to_string(),
            ParameterValue::Tag {
                value: deployed.into(),
                pattern: Some("1.27.*".into()),
                digest: None,
            },
        );
        dc.status = Some(DeployConfigStatus {
            parameters: status_params,
            config: Some(ShaMaybeBranch {
                sha: "c1".into(),
                branch: Some("master".into()),
            }),
            autodeploy: Some(autodeploy),
            orphaned: Some(false),
        });
        dc
    }

    fn nginx() -> ImageRef {
        ImageRef::parse("docker.io/library/nginx")
    }

    /// Tags as watchtower reads them: the version is the tag itself here.
    fn known(tags: &[&str]) -> Vec<Tag> {
        tags.iter()
            .map(|t| Tag {
                tag: t.to_string(),
                active: true,
                version: Some(t.to_string()),
                variant: None,
                build: None,
                history: vec![],
            })
            .collect()
    }

    fn arrives(tag: &str) -> Candidate<'_> {
        Candidate::new(tag, tag, None)
    }

    fn moves(
        dc: &DeployConfig,
        image: &ImageRef,
        tag: &str,
        deployed: &str,
    ) -> Result<String, Skip> {
        tag_parameter_for(dc, image, &arrives(tag), &known(&[deployed, tag]))
    }

    #[test]
    fn a_newer_tag_in_range_moves_a_tracked_parameter() {
        let dc = config(true, "1.27.2");
        assert_eq!(moves(&dc, &nginx(), "1.27.3", "1.27.2"), Ok("NGINX".into()));
        assert_eq!(
            moves(&dc, &nginx(), "1.27.1", "1.27.2"),
            Err(Skip::NotNewer("1.27.2".into()))
        );
        assert_eq!(
            moves(&dc, &nginx(), "1.28.0", "1.27.2"),
            Err(Skip::OutsideRange("1.27.*".into()))
        );
        assert_eq!(
            moves(&dc, &ImageRef::parse("library/redis"), "7.0.0", "1.27.2"),
            Err(Skip::OtherImage)
        );
        assert_eq!(
            moves(&config(false, "1.27.2"), &nginx(), "1.27.3", "1.27.2"),
            Err(Skip::Off)
        );
    }

    #[test]
    fn a_deployed_tag_watchtower_does_not_know_counts_as_older() {
        let dc = config(true, "1.27.2");
        assert_eq!(
            tag_parameter_for(&dc, &nginx(), &arrives("1.27.3"), &known(&["1.27.3"])),
            Ok("NGINX".into())
        );
    }

    #[test]
    fn a_followed_variant_moves_on_its_own_tags_only() {
        let mut dc = config(true, "2.1.2-alpine");
        if let Some(ParameterSource::Tag {
            pattern, variant, ..
        }) = dc.spec.spec.parameters.get_mut("NGINX")
        {
            *pattern = "^2.1.2".into();
            *variant = Some("alpine".into());
        }
        let known = |tags: &[&str]| -> Vec<Tag> {
            tags.iter()
                .map(|t| {
                    let (version, variant) = t.split_once('-').unwrap_or((t, ""));
                    Tag {
                        tag: t.to_string(),
                        active: true,
                        version: Some(version.to_string()),
                        variant: (!variant.is_empty()).then(|| variant.to_string()),
                        build: None,
                        history: vec![],
                    }
                })
                .collect()
        };
        let tags = known(&["2.1.2-alpine", "2.1.3-alpine", "2.1.3"]);
        assert_eq!(
            tag_parameter_for(
                &dc,
                &nginx(),
                &Candidate::new("2.1.3-alpine", "2.1.3", Some("alpine")),
                &tags
            ),
            Ok("NGINX".into())
        );
        assert_eq!(
            tag_parameter_for(
                &dc,
                &nginx(),
                &Candidate::new("2.1.3", "2.1.3", None),
                &tags
            ),
            Err(Skip::OutsideRange("^2.1.2".into()))
        );
    }

    #[test]
    fn two_part_versions_move_a_parameter_too() {
        let mut dc = config(true, "15.11");
        dc.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::track_pattern("^15.11", Durability::Standing),
        );
        assert_eq!(moves(&dc, &nginx(), "15.12", "15.11"), Ok("NGINX".into()));
        assert_eq!(
            moves(&dc, &nginx(), "15.11", "15.11"),
            Err(Skip::NotNewer("15.11".into()))
        );
    }

    #[test]
    fn pins_and_temporary_deployments_are_left_alone() {
        let mut dc = config(true, "1.27.2");
        dc.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::pin("1.27.2", Durability::Standing),
        );
        assert_eq!(moves(&dc, &nginx(), "1.27.3", "1.27.2"), Err(Skip::Pinned));

        let mut dc = config(true, "1.27.2");
        dc.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::track_pattern("1.*", Durability::Temporary),
        );
        assert_eq!(
            moves(&dc, &nginx(), "1.28.0", "1.27.2"),
            Err(Skip::Temporary)
        );
    }
}
