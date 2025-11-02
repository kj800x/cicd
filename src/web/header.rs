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

/// Common scripts for all pages
pub fn scripts() -> Markup {
    html! {
        script src="/assets/htmx.min.js" {}
        script src="/assets/idiomorph.min.js" {}
        script src="/assets/idiomorph-ext.min.js" {}
    }
}
