# Development Guide

This guide covers the architecture, patterns, and conventions used in the CI/CD Dashboard project.

## Architecture Overview

The application is built with:
- **Backend**: Rust + Actix Web
- **Frontend**: Maud (HTML templating) + HTMX (dynamic updates)
- **Database**: SQLite (via rusqlite)
- **Orchestration**: Kubernetes (Custom Resource Definitions)
- **VCS**: GitHub (via webhooks and API)

## Project Structure

```
src/
├── db/                     # Database layer
│   ├── migrations.rs       # Schema definitions
│   ├── git_repo.rs         # Repository entity
│   ├── git_commit.rs       # Commit entity
│   ├── deploy_config.rs    # Deploy config entity
│   └── ...                 # One file per entity
├── web/                    # HTTP handlers
│   ├── index.rs            # Home page
│   ├── deploy_configs.rs   # Deploy page: routes, the Action enum, resolution
│   ├── deploy_form.rs      # Deploy page left column (picker, action chooser, advanced form)
│   ├── preview.rs          # Deploy page right column (parameter rows, alerts, resources)
│   ├── patches.rs          # Patch list, the add-patch flow, patch routes
│   ├── blockers.rs         # Blockers page, held strip, blocker routes
│   ├── deploy_history.rs   # History feed, per-config history, revision detail, roll-back panel
│   ├── feed.rs             # Revision entries: verbs, tones, bursts, day groups
│   ├── home.rs             # Home: what needs a human
│   └── ...                 # One file per page/feature
├── kubernetes/             # Kubernetes integration
│   ├── controller.rs       # CRD reconciliation loop
│   ├── deploy_config.rs    # CRD definition
│   ├── deploy_handlers.rs  # Deploy/undeploy logic
│   └── webhook_handlers.rs # Config sync from repos
├── webhooks/               # GitHub webhook processing
│   ├── manager.rs          # Webhook multiplexing
│   ├── database.rs         # Commit/build tracking
│   └── config_sync.rs      # Deploy config sync
├── res/                    # Static resources
│   ├── styles.css          # Global styles
│   ├── deploy.css          # Deploy page styles
│   └── *.js                # HTMX + extensions
├── error.rs                # Error type definitions
├── lib.rs                  # Library root
└── main.rs                 # Binary entry point

kubernetes/                 # CRD, bootstrap manifests, migration/ scripts
```

## Key Patterns

### 1. Database Layer (Repository/DAO Pattern)

Each database entity has its own module with a struct and implementation block.

**Pattern:**
```rust
// src/db/my_entity.rs
use crate::error::{AppError, AppResult};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

pub struct MyEntity {
    pub id: i64,
    pub name: String,
    // ... other fields
}

impl MyEntity {
    // Convert database row to struct
    pub fn from_row(row: &rusqlite::Row) -> AppResult<Self> {
        Ok(MyEntity {
            id: row.get(0)?,
            name: row.get(1)?,
        })
    }

    // Retrieve single record
    pub fn get_by_name(
        name: &str,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Option<Self>> {
        let result = conn.prepare("SELECT id, name FROM my_entity WHERE name = ?1")?
            .query_row(params![name], |row| Ok(Self::from_row(row)))
            .optional()?
            .transpose()?;
        Ok(result)
    }

    // Retrieve multiple records
    pub fn get_all(
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Vec<Self>> {
        let mut stmt = conn.prepare("SELECT id, name FROM my_entity")?;
        let mut rows = stmt.query([])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(Self::from_row(row)?);
        }
        Ok(results)
    }

    // Insert or update
    pub fn upsert(entity: &MyEntity, conn: &PooledConnection<SqliteConnectionManager>) -> AppResult<()> {
        conn.prepare("INSERT OR REPLACE INTO my_entity (id, name) VALUES (?1, ?2)")?
            .execute(params![entity.id, entity.name])?;
        Ok(())
    }
}
```

**Key points:**
- All functions return `AppResult<T>` (type alias for `Result<T, AppError>`)
- Use `PooledConnection<SqliteConnectionManager>` for database access
- Use prepared statements with `params![]` macro
- Use `.optional()?` for queries that may return nothing
- Methods on the struct, not free functions

### 2. Web Layer (Page + Fragment Pattern)

The web layer uses a dual-endpoint pattern: one for full pages, one for fragments.

**Why?** HTMX can update parts of the page without full reloads. We serve:
- **Full page** on initial load
- **Fragments** for HTMX polling/updates

**Pattern:**
```rust
// Full page endpoint
#[get("/deploys")]
pub async fn deploy_configs(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to database");
        }
    };

    // ... fetch data ...

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                title { "Deploys" }
                link rel="stylesheet" href="/styles.css";
                script src="/htmx.min.js" {}
                script src="/idiomorph-ext.min.js" {}
            }
            body.deploy-page hx-ext="morph" {
                (header::render("deploys"))
                div.content {
                    div.deploy-grid
                        hx-get="/deploys-fragment"
                        hx-trigger="load, every 5s"
                        hx-swap="morph:innerHTML" {
                        // Initial content
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

// Fragment endpoint (for HTMX updates)
#[get("/deploys-fragment")]
pub async fn deploy_configs_fragment(
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return HttpResponse::InternalServerError().body("..."),
    };

    // ... fetch data ...

    let markup = html! {
        @for item in items {
            div.deploy-card {
                h3 { (item.name) }
                // ... item details ...
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
```

**Key points:**
- Page endpoint returns full HTML document
- Fragment endpoint returns just the content that updates
- Use `hx-get`, `hx-trigger`, and `hx-swap` for HTMX
- Idiomorph (`hx-ext="morph"`) for smooth DOM morphing
- Poll every 5 seconds: `hx-trigger="load, every 5s"`

### 3. CSS Organization

CSS is namespaced under page-level selectors to avoid conflicts.

**Pattern:**
```css
/* src/res/deploy.css */

/* Page-level container */
.deploy-page {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
}

/* Components within the page */
.deploy-page .deploy-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
    gap: 1rem;
}

.deploy-page .deploy-card {
    border: 1px solid #ddd;
    border-radius: 8px;
    padding: 1rem;
}

/* BEM-like naming for complex components */
.deploy-page .deploy-card__header {
    font-weight: bold;
}

.deploy-page .deploy-card__header--active {
    color: green;
}
```

**Key points:**
- Namespace everything under `.page-name` selector
- Use BEM-like naming: `.block__element--modifier`
- Keep global styles minimal (just layout utilities)
- One CSS file per major feature/page

### 4. Error Handling

**Rule: Always use `AppError` and `AppResult`, never `anyhow::Error`**

```rust
// Good
pub fn my_function() -> AppResult<String> {
    let data = some_operation()?;  // Propagate with ?
    Ok(data)
}

// Bad - don't use anyhow
pub fn my_function() -> anyhow::Result<String> {
    // ...
}
```

**AppError definition** (`src/error.rs`):
```rust
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Kubernetes error: {0}")]
    Kubernetes(#[from] kube::Error),

    // ... other variants
}

pub type AppResult<T> = Result<T, AppError>;
```

**Key points:**
- Use `?` operator for error propagation
- Avoid `.unwrap()` (linter enforces this)
- Use `.expect("reason")` only for truly impossible cases
- Log errors before returning them to users

### 5. HTMX Patterns

**Polling for live updates:**
```rust
html! {
    tbody hx-get="/endpoint-fragment"
         hx-trigger="load, every 5s"
         hx-swap="morph:innerHTML"
         hx-ext="morph" {
        // Content that updates every 5 seconds
    }
}
```

**Form submission:**
```rust
html! {
    form hx-post="/action"
         hx-swap="outerHTML" {
        input type="text" name="field";
        button type="submit" { "Submit" }
    }
}
```

**Click actions:**
```rust
html! {
    button hx-post="/deploy"
           hx-vals=(format!(r#"{{"name":"{}"}}"#, config_name))
           hx-swap="none" {
        "Deploy"
    }
}
```

### 6. Kubernetes Integration

**Custom Resource Definition:**
- Defined in `src/kubernetes/deploy_config.rs` using `kube` derive macros
- YAML manifest in `kubernetes/deploy-config-crd.yaml` (applied by hand; the Rust side has schema validation disabled)
- Controller watches for changes and reconciles

**Parameters:**
- `spec.parameters` is a map of named parameter sources, each tagged with `type`: `commit` (owner/repo/branch), `tag` (image/pattern), or `value` (default)
- `status.parameters` is the deployed value per parameter (`type: commit` → `value` is the SHA, plus optional `branch`)
- The `SHA` parameter is the legacy artifact repo; its value is substituted for `$SHA` in resource specs. `artifact_repository()` and `deployment_state()` read that key.
- On-disk `.deploy/<name>.yaml` files still use `artifactRepo`; `config_sync` maps it to `parameters.SHA`
- Types live in `src/kubernetes/parameters.rs`; see `kubernetes/example-deployconfig.yaml`

**Selections and durability:**
- `spec.selections.<PARAM>` records what a parameter follows: `track: {branch}` (an override), `pin: {value}`, or absent (the default channel). Each override has `durability: temporary | standing`, plus optional `note`, `by`, `since`
- Written by the deploy handler on branch and commit deploys and by the advanced form; config sync never touches it. "Latest" resolves through the selection; choosing "Track default" for `SHA` on the advanced form clears it (writing an explicit empty selection when the current one was derived from status)
- A config with any temporary override is a *temporary deployment*: badge, strip at the top of `/deploy`, and `CICD_TEMPORARY_DEPLOY=true`

**The deploy page (`/deploy`):**
- Simple mode is the config picker, the action chooser (End temporary deployment · Deploy · Deploy advanced · Redeploy previous · autodeploy toggle · Bounce · Execute job · Undeploy) and one button. "Deploy advanced" is an action: it unfolds one selector per parameter (track default / track another channel / pin), the patch list and a durability toggle that applies to every override made in that deploy. Only choices that differ from the current selection are recorded (`deploys::selection_changes`), so re-submitting a standing pin does not rewrite its durability
- Every choice lives in the query string (`action=deploy-advanced&sel_SHA=track&track_SHA=…&durability=…`); the GET form re-submits on change and the POST form mirrors it as hidden inputs. `preview::prepare` turns the action into the config as it would be after the deploy, and both columns render from that
- The preview prints one row per parameter, `NAME: current → new (channel)`, with `CONFIG` first; unchanged rows print one value. Tag parameters are resolved through watchtower on every render; an unresolvable row grows the one-shot input
- When the deploy moves the config commit, the preview reads the manifests at that commit to list resources that will be created or removed. `config_sync::fetch_deploy_configs_cached` memoises `.deploy/` per full sha for the process, so one GitHub fetch serves every poll after the first
- Blockers are created and cleared on `/blockers`; the deploy page only shows the held strip, the alert and disabled deploy actions

**History and home:**
- `/deploy-history` is a feed grouped by day (`web/feed.rs`): each revision is one sentence (verb, actor, reason) over the changes that matter in the `revision_diff` syntax. Consecutive autodeploys within ten minutes collapse into one disclosure. `/deploy-history/{name}` adds a state band (tracking, overrides, patches, blockers, autodeploy), marks the revision on the cluster, and offers Roll back on older deploy revisions; the confirmation opens beside the feed (`/fragments/rollback/{name}/{id}`) and posts a reason that becomes the blocker's. `/revisions/{id}` is the immutable detail page
- Revisions carry a `temporary` column so history can badge a temporary deploy without reconstructing selections
- `/` is home: blocked configs, temporary deployments, unhealthy deploys (the health page's check), configs drifted from latest (database for commits, one watchtower lookup per image for tags; the config commit counts only when the config's stored manifest hash differs between the deployed commit and the branch head, the preview's CONFIG CHANGED test, so editing one config in a shared repo does not drift the rest), configs whose declared tag pattern excludes the newest tag the image publishes ("Available upgrades": the same lookup asked for `*` beside each pattern; an upgrade needs a config change, so it is not drift and links to the declaration on GitHub), recent activity and standing overrides. Sections render only when non-empty; when the first three are empty the heading turns green over a four-fact band
- `parameters:` in `.deploy/<name>.yaml` declares parameters: `{type: commit, owner, repo, branch}` or `{type: value, default}`. `artifactRepo` is sugar for a commit parameter named `SHA`. Every parameter is substituted for `$NAME` in manifests; a declared parameter without a deployed value refuses to render
- Value parameters resolve to their pinned value or default at deploy time; set them from the advanced form ("Deploy advanced" on `/deploy`) or the `set_parameter` MCP tool
- "Redeploy previous" (deploy page action) replays the deployment before the current one: `Revision::previous_deploy_for` finds the newest deploy revision older than the config's latest revision (undeploys and patch changes are skipped), `normalize_action` fills the id into `Action::RedeployPrevious`, and the deploy replays that revision's values and patches exactly as a rollback does. Unlike a rollback it adds no blocker and is gated by blockers like any deploy; selections are untouched, so the next "latest" resumes what was being tracked. The revision is recorded with reason `redeploy of revision N`
- "End temporary deployment" (deploy page action, `end_temporary_deployment` MCP tool) clears every temporary selection and removes every temporary patch in one step, then deploys latest; standing overrides stay. `deploys::temporary_changes` computes what it would touch
- `spec.patches` are JSON Patch operations applied to rendered manifests after substitution, targeted by kind, name and optionally file. A patch that no longer fits fails the render loudly. On the web they are edited from the advanced form's patch list (the add flow picks a resource, then a knob such as replicas or an env var, and builds the JSON Patch; a custom form is one link away) and applied by the deploy: removals and additions ride along as `PatchChanges` in the query string (`patches=` JSON) and on the deploy request, are dry-run against Kubernetes when added and again in `run_action`, and are written with the deploy. The MCP `add_patch` / `remove_patch` tools still apply a change on their own
- Autodeploy (`src/webhooks/autodeploy.rs`): a successful check run on the branch a config's `SHA` parameter tracks deploys latest, unless the parameter is pinned, the config is a temporary deployment, or a blocker is active

**Tag parameters and watchtower:**
- `{type: tag, image: docker.io/library/nginx, pattern: "1.27.*"}` declares a parameter whose candidates are the image's tags as [watchtower](https://watchtower.home.coolkev.com/) reads them: it hands cicd each tag's `version` (two to four numeric components after an optional `v`: `15.11`, `1.27.3`, `4.0.19.2979`), `variant` (what follows the first `-`: `alpine`, `rc1`) and `build` (what watchtower splits off a linuxserver.io tag: the `-lsNN` build number and any glue upstream put on the version, so `12.0ubu2604-ls48` is version `12.0`, no variant, build `ubu2604.ls48`); a tag with no version (`latest`, `18`) never qualifies. The pattern is a semver range with Cargo's rules (`^1.27` admits 1.28; `~1.27` or `1.27.*` stay on 1.27; `=1.27.3` is that one). A two-part version is its `X.Y.0` and loses a tie to `X.Y.0` written out; a fourth component orders tags within a patch. Build metadata never affects matching; within one version the build orders rebuilds (`ls48` above `ls47`, compared as numbers) and any build outranks none, so linuxserver images resolve to their static `-lsNN` tag rather than the plain alias linuxserver calls unsupported. A variant is a prerelease a range never picks: `=2.1.2-alpine` pins one. To follow a variant, declare it: `{type: tag, image: docker.io/library/eclipse-mosquitto, pattern: "^2.1.2", variant: alpine}` sees only the `-alpine` tags, compared on the version before the dash, and renders the full tag. `src/kubernetes/tags.rs` holds the pure resolution
- Selections: `track: {pattern}` beside `track: {branch}`; pins are a tag. Status: `{type: tag, value, pattern?, digest?}`; the digest is informational
- `src/watchtower.rs` is the only client (`WATCHTOWER_URL`, default `http://watchtower.cicd.svc`). A deploy resolves tracked tag parameters through it after the static ones; pins, rollbacks, undeploys and every commit or value parameter never call it. Unreachable means the deploy fails closed with a 503 naming the parameter; the person types the tag for that one deploy (the box the preview grows under an unresolvable tag row, submitted as `value_<PARAM>`; `values` on the MCP deploy tool). The typed value is recorded under the tracked channel and the selection is not changed
- Registration: after every config sync, 15 s after start, and hourly, cicd reconciles watchtower's repo list to the images every config's tag parameters name (register, re-activate, and by default deactivate the rest). `CICD_WATCHTOWER_RECONCILE=full|activate-only|off`
- Events: `src/webhooks/tag_events.rs` polls `GET /api/events?after=<cursor>` every 30 s (cursor in the `watchtower_cursor` table; a first run starts from now) and runs the autodeploy gates for each added or moved tag

**Writers to the DeployConfig (server-side apply):**
- Every write to the custom resource goes through `src/kubernetes/cr_writers.rs` as a server-side apply under the manager that owns those keys: `cicd-config-sync` (parameters, config, kind, team; `status.orphaned`), `cicd-deploy` (`spec.specs`; `status.parameters`, `status.config`), `cicd-selections` (the whole selection map), `cicd-patches` (the patch list), `cicd-autodeploy` (`status.autodeploy`)
- A manager sends the complete set it owns every time; a key it stops sending is removed by the server. Never add a writer that sends part of what a manager owns
- Clear a map by omitting the key: the server rejects an owned empty map as null. Lists are atomic, so an empty list is fine
- A field set by anything else (`kubectl patch`, the merge-patch writers before this) stays until that owner releases it. `kubernetes/migration/adopt-field-managers.sh` rewrites `managedFields` once so existing objects belong to these managers; and a hand-set field must be removed by hand
- `cargo test --features test-crd -- --ignored live_writers` rehearses every writer against the cluster on a TestDeployConfig and asserts ownership and removal

**Environment variables injected into every container:**
- `CICD_DEPLOY_CONFIG`, `CICD_TEAM`, and `CICD_TEMPORARY_DEPLOY` (`true` while any temporary override or patch is active; the signal for refusing dangerous work such as schema migrations)
- Deliberately nothing that changes with every deploy: the parameter values and the config commit used to be injected too, which rolled every Deployment on every deploy, including deploys that only touched an Ingress or a Secret. A workload's image tag already carries its version

**Controller pattern:**
```rust
async fn reconcile(dc: Arc<DeployConfig>, ctx: Arc<Context>) -> Result<Action> {
    // 1. Read desired state from CRD
    // 2. Apply resources to cluster
    // 3. Prune old resources
    // 4. Update status
    Ok(Action::requeue(Duration::from_secs(5)))
}
```

## Development Workflow

### 1. Running Locally

```bash
# Install dependencies (requires Rust toolchain)
cargo build

# Run the server
cargo run

# Run with logging
RUST_LOG=info cargo run

# Run tests
cargo test
```

### 2. Database Migrations

Migrations are in `src/db/migrations.rs`. To add a new migration:

```rust
let migrations: Migrations = Migrations::new(vec![
    M::up(indoc! { r#"
        /* existing schema */
    "#}),
    M::up(indoc! { r#"
        /* new migration */
        ALTER TABLE my_table ADD COLUMN new_field TEXT;
    "#}),
]);
```

**Important:** Never modify existing migrations. Always append new ones.

### 3. Adding a New Page

1. Create `src/web/my_page.rs`
2. Define page and fragment handlers
3. Add routes in `src/main.rs`
4. Create CSS file in `src/res/` if needed
5. Update `serve_static_file!` macro if serving new CSS

```rust
// src/web/my_page.rs
use crate::prelude::*;

#[get("/my-page")]
pub async fn my_page(/*...*/) -> impl Responder {
    // Full page HTML
}

#[get("/my-page-fragment")]
pub async fn my_page_fragment(/*...*/) -> impl Responder {
    // Fragment HTML
}
```

```rust
// src/main.rs
.service(my_page)
.service(my_page_fragment)
```

### 4. Code Quality Checks

Before committing:

```bash
# Check compilation
cargo check

# Run linter
cargo clippy

# Format code
cargo fmt

# Run tests
cargo test
```

## Common Gotchas

1. **Don't use `unwrap()` or `expect()` without good reason** - Linter enforces this
2. **Always namespace CSS** - Prevents style conflicts
3. **Use `?` for error propagation** - Don't return errors manually
4. **Database migrations are append-only** - Never modify existing ones
5. **HTMX requires proper content-type** - Always set `"text/html; charset=utf-8"`
6. **Pool connections must be dropped** - Don't hold them across await points

## Testing

### Integration Tests
Tests go in `tests/` directory (not yet implemented).

### Rendering the pages without a cluster
`src/web/fixtures.rs` renders the deploy page in several states (simple, advanced, held, temporary, watchtower down, undeploy) and the blockers page from synthetic data into static HTML with the stylesheets inlined:

```bash
CICD_FIXTURE_DIR=/tmp/cicd-pages cargo test fixtures -- --ignored
```

Open the files in a browser to check a CSS or layout change against the design.

### Manual Testing Checklist
- [ ] Bootstrap feature syncs repos correctly
- [ ] Deploy/undeploy flows work
- [ ] HTMX polling updates without page refresh
- [ ] Error messages display clearly
- [ ] Database migrations apply cleanly

## Running a dev controller against the cluster (`test-crd`)

The `test-crd` feature builds a controller that manages `TestDeployConfig`
resources instead of `DeployConfig`, so it can run on a dev machine against the
real cluster while production keeps running. In test mode the binary also
prefixes every namespace with `test-crd-`, skips `Ingress` manifests, and does
not report deployments to GitHub. It connects to the webhook proxy exactly as
production does; the proxy fans events out to every reader, and reacting to
webhooks is most of what the controller does, so the dev instance sees the
same pushes production sees.

```bash
kubernetes/test-crd/regenerate.sh                                  # after any CRD change
kubectl apply -f kubernetes/test-crd/test-deploy-config-crd.yaml   # once, and after regenerating
DATABASE_PATH=./dev.db ENABLE_K8S_CONTROLLER=true \
  WEBSOCKET_URL=... CLIENT_SECRET=... \
  cargo run --features test-crd
```

Every push to a config repo's default branch then creates or updates a
TestDeployConfig, and its workloads land in `test-crd-<namespace>`. Use the
bootstrap page for repos you want synced before their next push. Do not deploy a
service whose production copy shares a cluster-wide name (an Ingress host, a
NodePort) unless you know the collision is harmless. Tear down with
`kubectl delete tdc -A --all`; owner references remove the workloads, then delete
the `test-crd-*` namespaces.

## Deployment

1. Build binary: `cargo build --release`
2. Apply CRD: `kubectl apply -f kubernetes/deploy-config-crd.yaml`
3. Deploy application with access to kubeconfig
4. Set environment variables:
   - `WEBSOCKET_URL` - GitHub webhook proxy
   - `CLIENT_SECRET` - Webhook authentication
   - `CICD_DISABLE_PRUNE` - When `true`, reconcile logs stale owned resources instead of deleting them (migration safety valve)
   - `GITHUB_APP_ID` / `GITHUB_APP_PRIVATE_KEY` - GitHub App credentials (see README)
   - `DATABASE_PATH` - SQLite database path (optional, defaults to "db.db")
   - `TEMPLATE_NAMESPACE` - Template namespace for resource copying (optional)

## Resources

- [Actix Web Docs](https://actix.rs/)
- [Maud Docs](https://maud.lambda.xyz/)
- [HTMX Docs](https://htmx.org/)
- [Kube-rs Docs](https://kube.rs/)
- [Rusqlite Docs](https://docs.rs/rusqlite/)

