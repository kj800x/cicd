use maud::{html, Markup};

/// Renders the main header and subheader with navigation
pub fn render(active_page: &str) -> Markup {
    html! {
        header {
            div class="header" {
                span class="header-logo" { "Homelab" }
            }
            div class="subheader" {
                a href="#" class="subheader-brand" {
                    "CI / CD"
                }
                div class="subheader-nav" {
                    a href="/deploy" class=(if active_page == "deploy" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Deploy" }
                    a href="/watchdog" class=(if active_page == "watchdog" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Watchdog" }
                    a href="/" class=(if active_page == "branches" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Recent branches" }
                    a href="/all-recent-builds" class=(if active_page == "builds" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Recent builds" }
                }
            }
        }
    }
}

/// Returns a link tag for the common stylesheet
pub fn stylesheet_link() -> Markup {
    html! {
        link rel="stylesheet" href="/assets/styles.css";
    }
}

/*
Fragment markup:

<div class="build-status-container" hx-get="/fragments/build-status/home-sensors/manual-deploy-config?action=deploy" hx-trigger="load, every 2s"><div class="alert alert-danger"><div class="alert-header">New builds failed</div><div class="alert-content">The following commit failed to build:branch-test:<span><a class="git-ref" href="https://github.com/kj800x/test-repo/tree/ec75d9ddd8dccdcb780e385bd0145f343d862f70">ec75d9d</a></span><commit class="timestamp">Committed at <time datetime="2025-04-07T05:15:48+00:00">April 07 at 01:15 AM ET</time></commit><a href="https://github.com/kj800x/test-repo/commit/ec75d9ddd8dccdcb780e385bd0145f343d862f70/checks">Build log</a><pre class="commit-message">break the build</pre></div></div></div>

*/

/// Common scripts for all pages
pub fn scripts() -> Markup {
    html! {
        script src="/assets/htmx.min.js" {}
        script src="/assets/idiomorph.min.js" {}
        script src="/assets/idiomorph-ext.min.js" {}
    }
}
