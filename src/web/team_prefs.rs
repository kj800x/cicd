use std::collections::HashSet;

use actix_web::HttpRequest;

use crate::kubernetes::DeployConfig;

pub const TEAMS_COOKIE: &str = "teams";

/// User team preferences stored in a cookie.
/// None means no cookie present; Some(empty) means cookie present but empty.
#[derive(Clone, Debug, Default)]
pub struct TeamsCookie(pub Option<HashSet<String>>);

impl TeamsCookie {
    /// Build from the incoming request's cookie.
    pub fn from_request(req: &HttpRequest) -> Self {
        let cookie = match req.cookie(TEAMS_COOKIE) {
            Some(c) => c,
            None => return TeamsCookie(None),
        };
        let raw = cookie.value().trim();
        let set: HashSet<String> = if raw.is_empty() {
            HashSet::new()
        } else {
            raw.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        TeamsCookie(Some(set))
    }

    /// Serialize to a stable, comma-separated value for the cookie.
    /// If there is no cookie (None), serialize to empty string.
    pub fn serialize(&self) -> String {
        match &self.0 {
            None => "".to_string(),
            Some(set) => {
                let mut v: Vec<String> = set.iter().cloned().collect();
                v.sort_unstable();
                v.join(",")
            }
        }
    }

    /// Returns true if the given team is present in the cookie.
    /// Absence of cookie counts as false for membership.
    pub fn is_member(&self, team: &str) -> bool {
        match &self.0 {
            None => false,
            Some(set) => set.contains(team),
        }
    }

    /// Returns a filtered Vec of DeployConfigs based on cookie contents.
    /// - No cookie -> empty
    /// - Empty cookie -> empty
    /// - Otherwise only configs whose team is present.
    pub fn filter_configs(&self, configs: &[DeployConfig]) -> Vec<DeployConfig> {
        match &self.0 {
            None => Vec::new(),
            Some(set) if set.is_empty() => Vec::new(),
            Some(set) => configs
                .iter()
                .filter(|cfg| set.contains(cfg.team()))
                .cloned()
                .collect(),
        }
    }
}
