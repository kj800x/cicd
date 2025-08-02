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

/// Common CSS styles for header and navigation
pub fn styles() -> &'static str {
    r#"
    .header {
        background-color: var(--header-bg);
        color: white;
        padding: 8px 16px;
        display: flex;
        align-items: center;
    }
    .header-logo {
        margin-right: 12px;
    }
    .header-nav {
        display: flex;
        gap: 16px;
        margin-left: 24px;
    }
    .header-nav-item {
        color: rgba(255, 255, 255, 0.7);
        text-decoration: none;
        font-size: 14px;
        font-weight: 600;
        padding: 8px 8px;
    }
    .header-nav-item:hover, .header-nav-item.active {
        color: white;
    }
    .subheader {
        border-bottom: 1px solid var(--border-color);
        display: flex;
        padding: 0 16px;
    }
    .subheader-brand {
        display: flex;
        align-items: center;
        padding: 12px 0;
        margin-right: 24px;
        color: var(--text-color);
        font-weight: 600;
        text-decoration: none;
    }
    .subheader-brand img {
        margin-right: 8px;
    }
    .subheader-nav {
        display: flex;
    }
    .subheader-nav-item {
        color: var(--text-color);
        text-decoration: none;
        padding: 12px 16px;
        font-size: 14px;
        border-bottom: 2px solid transparent;
    }
    .subheader-nav-item:hover {
        border-bottom-color: #d0d7de;
    }
    .subheader-nav-item.active {
        border-bottom-color: var(--primary-blue);
        font-weight: 500;
    }

    /* Styles for fragments */

    .alert {
        padding: 16px;
        border: 2px solid grey;
        margin: 16px 0;
        width: 100%;
        box-sizing: border-box;
        word-wrap: break-word;
        overflow-wrap: break-word;
    }
    .alert-header {
        font-size: 1.4em;
        margin-bottom: 4px;
        word-wrap: break-word;
        overflow-wrap: break-word;
    }
    .alert-danger {
        background-color: #fdedee;
        border-color: #f8a9ad;
    }
    .alert-warning {
        background-color: #fef8f0;
        border-color: #fae0b5;
    }
    .alert-success {
        background-color: #e5f8f6;
        border-color: #7fded2;
    }
    .alert-content .details {
        display: flex;
        flex-direction: column;
        gap: 4px;
        word-wrap: break-word;
        overflow-wrap: break-word;
    }
    .alert-content .details {
        font-size: 12px;
    }
    .alert-content .commit-message {
        font-size: 12px;
        padding-left: 16px;
        border-left: 4px solid #000;
        border-left-color: #dfe3eb;
    }
    .alert-content .details a {
        color: inherit !important;
    }
    .alert-content pre.commit-message {
        white-space: pre-wrap;       /* Since CSS 2.1 */
        white-space: -moz-pre-wrap;  /* Mozilla, since 1999 */
        white-space: -pre-wrap;      /* Opera 4-6 */
        white-space: -o-pre-wrap;    /* Opera 7 */
        word-wrap: break-word;       /* Internet Explorer 5.5+ */
        overflow-wrap: break-word;   /* Modern browsers */
        max-width: 100%;
        box-sizing: border-box;
    }
    "#
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
