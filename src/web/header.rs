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
    "#
}

/// Common scripts for all pages
pub fn scripts() -> Markup {
    html! {
        script src="/assets/htmx.min.js" {}
    }
}
