# CI/CD Dashboard Todo & Roadmap

## High Priority (Post-Cutover)

### Deleted Config Cleanup 🔴
**Status:** Critical bug
**File:** `src/kubernetes/webhook_handlers.rs` lines 138-150

Deploy configs that are deleted from the `.deploy/` directory in repos are not properly cleaned up in Kubernetes. The current logic doesn't detect deletions correctly.

**Fix needed:**
- Use Kubernetes API to determine actual state instead of relying solely on database
- Properly mark configs as orphaned when deleted from source
- Ensure orphaned configs are eventually cleaned up

---

## Core Features (Roadmap)

### 1. Autodeploy Automation 🎯 TOP PRIORITY
**Status:** Partially implemented (toggle exists, automation missing)

The UI to toggle autodeploy exists and the status field is tracked, but the actual automation is not implemented.

**What's needed:**
- Webhook handler to listen for successful build completions
- Automatically trigger deploy when:
  - Build succeeds on tracked branch
  - Autodeploy is enabled for the config
  - Config references that artifact repo/branch

### 2. Orphaned Feature Completion
**Status:** Core logic done, edge cases and UI missing

**Completed:**
- Configs marked orphaned when deleted but still deployed ✅
- Undeploy deletes orphaned configs ✅

**TODO:**
- Add UI alert box/banner when viewing an orphaned config
- Test edge cases:
  - Orphan a config, then undeploy → should delete ✅ (verify)
  - Undeploy a config, then orphan → should delete (test)
  - Orphan → deploy change → undeploy → ? (define behavior)
- Decision: Block deploys to orphaned configs or allow?

### 3. Advanced Deploy Features

#### Health-Based Deploy Success
Don't mark a deploy as successful until all child resources are healthy and ready.

#### Auto-Rollback on Timeout
If a deploy doesn't succeed within a configured time, automatically roll back to the previous version.

#### Deploy Revert
Allow users to revert to any previous successful deploy from the history page.

#### Config SHA Tracking Enhancement
- Show config SHA separately in UI
- Flag deploys that include config changes (not just artifact changes)
- Use `config_version_hash` from database

### 4. Enhanced Resource Status & Diff View
**Status:** Initial version complete ✅, enhancements planned

**Current:** Child resources shown with status indicators

**Planned enhancements:**
- Show actual resource manifest YAML that will be deployed
- Indicate which resources use `$SHA` template (will change on artifact updates)
- Show YAML diff for resources that will change
- Indicate which resources have no changes (e.g., "Service unchanged")
- Pre-deploy validation/dry-run view

### 5. Watchdog / System Health Dashboard
**Context:** Previous implementation had a system-wide health summary page

**Scope:**
- Deploys out of date (artifact newer than deployed version)
- Master branch builds failing
- Pods in failing states across all configs
- Other system-wide alerts

**Note:** Needs design work to implement efficiently (avoid N+1 queries, caching strategy)

---

## UI Improvements

- [ ] Show `kind` field in deploy config dropdowns and detail screens
- [ ] Group by team, hide other teams' resources (privacy/focus)
- [ ] Fix exact version deploy "from→to" display
- [ ] Test branch deployment flows when GitHub Actions fixed
- [ ] Link to Headlamp instead of (or in addition to) Kubernetes dashboard

---

## Features Under Consideration

### GraphQL API
**Status:** Not implemented (only package imports exist)

The README describes a GraphQL API but it's not actually implemented. The codebase uses direct database queries and REST-style endpoints instead.

**Decision:** Not a priority. May implement in the future if there's a compelling use case for a query language over the current approach.

### Discord Notifications
**Status:** Removed during rewrite

Previous implementation had Discord notifications for build events, but they were noisy and not particularly useful.

**Decision:** Feature removed. May revisit with better implementation if we can make notifications actionable and relevant.

---

## Code Quality & Maintenance

### Error Handling Consistency
**Goal:** Use only `AppError`, remove all `anyhow::Error` usage

**Files to audit:**
- `src/webhooks/config_sync.rs` (uses anyhow)
- Any other files returning `anyhow::Result`

### Code Organization
- [ ] Ensure code is in the right files
- [ ] Ensure methods are on appropriate structs vs bare functions
- [ ] Audit `DeployedOnlyConfig` code paths
- [ ] Audit `expect()` and `unwrap()` usage
- [ ] Review `#[allow()]` attributes (are they still needed?)
- [ ] Ensure database modules are consistent in structure

### Minor Improvements
- [ ] Fix "build time" labels (should be "commit time")
- [ ] Move `extract_branch_name` onto `PushEvent` as method
- [ ] Fetch `check_name` from GitHub API properly (currently hardcoded "default")
- [ ] Consider renaming `deploy_handlers.rs` to `deploy_action.rs`
- [ ] Centralize Kube Client initialization (currently initialized multiple times)
- [ ] Make LogHandler conditionally enabled via env vars

---

## Testing

### Integration Tests Needed
- [ ] Repos without `.deploy/` directory (should not error)
- [ ] Deploy configs removed from repos (should be orphaned/deleted)
- [ ] Namespace changes (should fail or be prevented)
- [ ] Artifact repo missing from database (should error clearly)

### Manual Testing Before Cutover
- [ ] Bootstrap feature works correctly
- [ ] Deploy/undeploy flows work
- [ ] Autodeploy toggle persists correctly
- [ ] Orphaned configs shown in UI
- [ ] Deploy history records all events
