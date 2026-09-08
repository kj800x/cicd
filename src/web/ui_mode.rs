//! A temporary switch that hides everything the parameters redesign added
//! to the pages, so the site shows only the original, intentionally
//! designed UI. Meant for pointing a design tool at the site to extract
//! its design language before the new features get a proper design pass.
//!
//! Set `CICD_UI_CLASSIC=true` on the controller. Logic is untouched: every
//! action and API still works, MCP is unaffected, only the markup for the
//! new features is skipped. Remove the variable to bring them back.

use std::sync::OnceLock;

static CLASSIC: OnceLock<bool> = OnceLock::new();

pub fn classic() -> bool {
    *CLASSIC.get_or_init(|| {
        std::env::var("CICD_UI_CLASSIC")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}
