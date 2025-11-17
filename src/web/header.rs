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
                    a href="/deploy-history" class=(if active_page == "history" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Deploy history" }
                    // a href="/watchdog" class=(if active_page == "watchdog" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Watchdog" }
                    a href="/branches" class=(if active_page == "branches" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Recent branches" }
                    a href="/all-recent-builds" class=(if active_page == "builds" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Recent builds" }
                    a href="/settings" class=(if active_page == "settings" { "subheader-nav-item active" } else { "subheader-nav-item" }) {
                        span {
                            i class="fa fa-gear" {}
                        }
                    }
                }
            }
        }
    }
}

/// Returns a link tag for the common stylesheet
pub fn stylesheet_link() -> Markup {
    html! {
        link rel="stylesheet" href="/res/styles.css";
        link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/font-awesome/4.4.0/css/font-awesome.css";
        link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/octicons/3.1.0/octicons.css";
    }
}

/// Common scripts for all pages
pub fn scripts() -> Markup {
    html! {
        script src="/res/htmx.min.js" {}
        script src="/res/idiomorph.min.js" {}
        script src="/res/idiomorph-ext.min.js" {}
    }
}
