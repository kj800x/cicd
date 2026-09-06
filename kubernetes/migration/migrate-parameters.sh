#!/usr/bin/env bash
# Emergency forward fill: copy the legacy single-artifact fields of every
# DeployConfig into the parameters maps.
#   spec.artifact   {owner, repo, branch} -> spec.parameters.SHA   {type: commit, owner, repo, branch}
#   status.artifact {sha, branch}         -> status.parameters.SHA {type: commit, value, branch}
#
# The dual controller (release B) does this itself on reconcile. Use this
# script only for a straggler the controller cannot reach, or to fill in after
# restoring from a backup taken before the migration.
#
# Idempotent: a config whose SHA entry already exists is skipped. Old fields
# are never removed here; the contracted CRD prunes them.
# Requires the CRD to declare the parameters fields (expanded or later).
#
# Usage: migrate-parameters.sh [--dry-run] [--only ns/name]
# Env:   CRD_NAME (default deployconfigs.cicd.coolkev.com)
set -euo pipefail

CRD_NAME="${CRD_NAME:-deployconfigs.cicd.coolkev.com}"
DRY_RUN=0; ONLY=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --only) ONLY="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

for tool in kubectl jq; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done
kubectl_minor=$(kubectl version --client -o json | jq -r '.clientVersion.minor' | tr -dc '0-9')
if [[ -z "$kubectl_minor" || "$kubectl_minor" -lt 24 ]]; then
  echo "kubectl >= 1.24 required for --subresource=status (found minor '$kubectl_minor')" >&2
  exit 1
fi

run() {
  if [[ "$DRY_RUN" -eq 1 ]]; then printf 'DRY RUN:'; printf ' %q' "$@"; printf '\n'
  else printf '+'; printf ' %q' "$@"; printf '\n'; "$@"; fi
}

kubectl get "$CRD_NAME" -A -o json | jq -c '.items[]' | while IFS= read -r item; do
  ns=$(jq -r '.metadata.namespace' <<<"$item")
  name=$(jq -r '.metadata.name' <<<"$item")
  if [[ -n "$ONLY" && "$ONLY" != "$ns/$name" ]]; then continue; fi

  spec_patch=$(jq -c '
    if (.spec.artifact != null) and ((.spec.parameters // {}).SHA == null)
    then {spec: {parameters: {SHA: ({type: "commit"} + .spec.artifact)}}}
    else empty end' <<<"$item")
  status_patch=$(jq -c '
    if (.status.artifact != null) and ((.status.parameters // {}).SHA == null)
    then {status: {parameters: {SHA: (
      {type: "commit", value: .status.artifact.sha}
      + (if .status.artifact.branch != null then {branch: .status.artifact.branch} else {} end)
    )}}}
    else empty end' <<<"$item")

  if [[ -z "$spec_patch" && -z "$status_patch" ]]; then
    echo "= $ns/$name: nothing to do"; continue
  fi
  [[ -n "$spec_patch" ]] && run kubectl -n "$ns" patch "$CRD_NAME" "$name" --type=merge -p "$spec_patch"
  [[ -n "$status_patch" ]] && run kubectl -n "$ns" patch "$CRD_NAME" "$name" --subresource=status --type=merge -p "$status_patch"
done
