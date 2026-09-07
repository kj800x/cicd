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
│   ├── deploy_configs.rs   # Deploy management UI
│   ├── deploy_history.rs   # Deploy history page
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
- `spec.parameters` is a map of named parameter sources, each tagged with `type` (currently only `commit`: owner/repo/branch)
- `status.parameters` is the deployed value per parameter (`type: commit` → `value` is the SHA, plus optional `branch`)
- The `SHA` parameter is the legacy artifact repo; its value is substituted for `$SHA` in resource specs. `artifact_repository()` and `deployment_state()` read that key.
- On-disk `.deploy/<name>.yaml` files still use `artifactRepo`; `config_sync` maps it to `parameters.SHA`
- Types live in `src/kubernetes/parameters.rs`; see `kubernetes/example-deployconfig.yaml`

**Selections and durability:**
- `spec.selections.<PARAM>` records what a parameter follows: `track: {branch}` (an override), `pin: {value}`, or absent (the default channel). Each override has `durability: temporary | standing`, plus optional `note`, `by`, `since`
- Written by the deploy handler on branch and commit deploys; config sync never touches it. "Latest" resolves through the selection; "Back to default branch" clears it
- A config with any temporary override is a *temporary deployment*: badge, strip at the top of `/deploy`, and `CICD_TEMPORARY_DEPLOY=true`

**Value parameters, patches and autodeploy:**
- `parameters:` in `.deploy/<name>.yaml` declares parameters: `{type: commit, owner, repo, branch}` or `{type: value, default}`. `artifactRepo` is sugar for a commit parameter named `SHA`. Every parameter is substituted for `$NAME` in manifests; a declared parameter without a deployed value refuses to render
- Value parameters resolve to their pinned value or default at deploy time; set them from the Parameters panel or the `set_parameter` MCP tool
- "End temporary deployment" (deploy page action, `end_temporary_deployment` MCP tool) clears every temporary selection and removes every temporary patch in one step, then deploys latest; standing overrides stay. `deploys::temporary_changes` computes what it would touch
- `spec.patches` are JSON Patch operations applied to rendered manifests after substitution, targeted by kind, name and optionally file. A patch that no longer fits fails the render loudly. Manage them from the Patches panel or `add_patch` / `remove_patch`
- Autodeploy (`src/webhooks/autodeploy.rs`): a successful check run on the branch a config's `SHA` parameter tracks deploys latest, unless the parameter is pinned, the config is a temporary deployment, or a blocker is active

**Writers to the DeployConfig (server-side apply):**
- Every write to the custom resource goes through `src/kubernetes/cr_writers.rs` as a server-side apply under the manager that owns those keys: `cicd-config-sync` (parameters, config, kind, team; `status.orphaned`), `cicd-deploy` (`spec.specs`; `status.parameters`, `status.config`), `cicd-selections` (the whole selection map), `cicd-patches` (the patch list), `cicd-autodeploy` (`status.autodeploy`)
- A manager sends the complete set it owns every time; a key it stops sending is removed by the server. Never add a writer that sends part of what a manager owns
- Clear a map by omitting the key: the server rejects an owned empty map as null. Lists are atomic, so an empty list is fine
- A field set by anything else (`kubectl patch`, the merge-patch writers before this) stays until that owner releases it. `kubernetes/migration/adopt-field-managers.sh` rewrites `managedFields` once so existing objects belong to these managers; and a hand-set field must be removed by hand
- `cargo test --features test-crd -- --ignored live_writers` rehearses every writer against the cluster on a TestDeployConfig and asserts ownership and removal

**Environment variables injected into every container:**
- `CICD_DEPLOY_CONFIG`, `CICD_TEAM`
- `CICD_TEMPORARY_DEPLOY`: `true` while a temporary override is active. Apps refuse dangerous work (schema migrations) on it; a standing pin does not trip it
- `CICD_PARAM_<NAME>`, `CICD_PARAM_<NAME>_MODE` (`track`, `override`, `pin`), `CICD_PARAM_<NAME>_CHANNEL` (absent when pinned)
- `CICD_CONFIG_SHA`, `CICD_CONFIG_BRANCH`

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

