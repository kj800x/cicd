#!/usr/bin/env bash
# Recreate DeployConfigs from a backup made by backup-deploy-configs.sh.
#
# Usage: restore-deploy-configs.sh <backup dir> [--dry-run] [--only ns/name]
# Env:   CRD_NAME (default deployconfigs.cicd.coolkev.com)
#
# For each object: apply spec (server-assigned metadata stripped), then patch
# the status subresource with the saved status. Restore the CRD first with
#   kubectl apply -f <backup dir>/crd.yaml
# if the schema was contracted after the backup was taken.
#
# Stop the controller before restoring (kubectl -n cicd scale deploy cicd --replicas=0)
# so it does not act on half-restored objects.
set -euo pipefail

CRD_NAME="${CRD_NAME:-deployconfigs.cicd.coolkev.com}"
DIR="${1:?backup dir required}"; shift
DRY_RUN=0; ONLY=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --only) ONLY="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

run() {
  if [[ "$DRY_RUN" -eq 1 ]]; then printf 'DRY RUN:'; printf ' %q' "$@"; printf '\n'
  else printf '+'; printf ' %q' "$@"; printf '\n'; "$@"; fi
}

jq -c '.items[]' "$DIR/deployconfigs.json" | while IFS= read -r item; do
  ns=$(jq -r '.metadata.namespace' <<<"$item")
  name=$(jq -r '.metadata.name' <<<"$item")
  if [[ -n "$ONLY" && "$ONLY" != "$ns/$name" ]]; then continue; fi

  spec_obj=$(jq -c 'del(.status)
    | .metadata |= {name, namespace, labels, annotations}
    | .metadata |= with_entries(select(.value != null))' <<<"$item")
  status=$(jq -c '.status // empty' <<<"$item")

  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "DRY RUN: apply $ns/$name; patch status: ${status:-<none>}"
    continue
  fi
  echo "+ apply $ns/$name"
  kubectl apply -f - <<<"$spec_obj" >/dev/null
  if [[ -n "$status" ]]; then
    run kubectl -n "$ns" patch "$CRD_NAME" "$name" --subresource=status --type=merge -p "{\"status\":$status}" >/dev/null
  fi
done
echo "Restore complete"
