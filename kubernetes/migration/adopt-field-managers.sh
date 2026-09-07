#!/usr/bin/env bash
# Move ownership of every DeployConfig's fields to the server-side-apply
# field managers the controller writes with, once.
#
# Before this, fields were written by merge patches and are owned by legacy
# managers ("unknown", "kubectl-patch", "before-first-apply"). Server-side
# apply never removes a field another manager still owns, so until those
# entries are replaced a key the controller stops sending (a parameter
# removed from the repo, a cleared selection) would linger. This script
# rewrites metadata.managedFields so that each field belongs to exactly the
# manager that will keep writing it:
#
#   cicd-config-sync  spec.parameters spec.config spec.kind spec.team | status.orphaned
#   cicd-deploy       spec.specs                                      | status.parameters status.config
#   cicd-selections   spec.selections
#   cicd-patches      spec.patches
#   cicd-autodeploy                                                   | status.autodeploy
#
# Run after the controller that uses these managers is deployed. Idempotent.
# Usage: adopt-field-managers.sh [--dry-run]
#   CRD_PLURAL  defaults to deployconfigs.cicd.coolkev.com
#               (testdeployconfigs.cicd.coolkev.com for the test-crd feature)
set -euo pipefail

DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true
CRD_PLURAL="${CRD_PLURAL:-deployconfigs.cicd.coolkev.com}"

JQ_PROGRAM='
# fieldsV1 for a value: granular maps recurse, atomic lists and scalars are leaves.
def fv: if type=="object" then ({".":{}} + (to_entries | map({key:("f:"+.key), value:(.value|fv)}) | from_entries)) else {} end;
def entry($m; $sub; $f): {manager:$m, operation:"Apply", apiVersion:"cicd.coolkev.com/v1", time:(now|todate), fieldsType:"FieldsV1", fieldsV1:$f} + (if $sub then {subresource:"status"} else {} end);
def pick($obj; $keys): ($obj // {}) | with_entries(select(.key as $k | $keys | index($k))) | to_entries | map({key:("f:"+.key), value:(.value|fv)}) | from_entries;
. as $o
| [
  entry("cicd-config-sync"; false; {"f:spec": ({".":{}} + pick($o.spec; ["parameters","config","kind","team"]))}),
  entry("cicd-deploy";      false; {"f:spec": pick($o.spec; ["specs"])}),
  entry("cicd-selections";  false; {"f:spec": pick($o.spec; ["selections"])}),
  entry("cicd-patches";     false; {"f:spec": pick($o.spec; ["patches"])}),
  entry("cicd-config-sync"; true;  {"f:status": ({".":{}} + pick($o.status; ["orphaned"]))}),
  entry("cicd-deploy";      true;  {"f:status": pick($o.status; ["parameters","config"])}),
  entry("cicd-autodeploy";  true;  {"f:status": pick($o.status; ["autodeploy"])})
] | map(select((.fieldsV1["f:spec"] // .fieldsV1["f:status"] | del(.["."]) | length) > 0))
| {metadata:{managedFields:.}}
'

items=$(kubectl get "$CRD_PLURAL" -A --show-managed-fields -o json | jq -c '.items[]')
count=0
while IFS= read -r obj; do
  [[ -z "$obj" ]] && continue
  ns=$(jq -r '.metadata.namespace' <<<"$obj")
  name=$(jq -r '.metadata.name' <<<"$obj")
  before=$(jq -r '[.metadata.managedFields[]? | .manager + (if .subresource then "/status" else "" end)] | unique | join(",")' <<<"$obj")
  patch=$(jq "$JQ_PROGRAM" <<<"$obj")
  after=$(jq -r '[.metadata.managedFields[] | .manager + (if .subresource then "/status" else "" end)] | unique | join(",")' <<<"$patch")
  if $DRY_RUN; then
    printf '%s/%s\n  before: %s\n  after:  %s\n' "$ns" "$name" "$before" "$after"
  else
    kubectl -n "$ns" patch "$CRD_PLURAL" "$name" --type=merge -p "$patch" >/dev/null
    now=$(kubectl -n "$ns" get "$CRD_PLURAL" "$name" --show-managed-fields -o json | jq -r '[.metadata.managedFields[] | .manager + (if .subresource then "/status" else "" end)] | unique | join(",")')
    printf '%s/%s: %s\n' "$ns" "$name" "$now"
  fi
  count=$((count + 1))
done <<<"$items"
echo "$count object(s) $($DRY_RUN && echo 'inspected (dry run)' || echo 'adopted')"
