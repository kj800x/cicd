#!/usr/bin/env bash
# Snapshot every DeployConfig (spec and status) and the CRD itself.
#
# Usage: backup-deploy-configs.sh [dir]     (default: backup/<UTC timestamp>)
# Env:   CRD_NAME (default deployconfigs.cicd.coolkev.com)
#
# The status subresource is the only record of what is currently deployed,
# so run this before every schema step. Restore with restore-deploy-configs.sh.
set -euo pipefail

CRD_NAME="${CRD_NAME:-deployconfigs.cicd.coolkev.com}"
DIR="${1:-backup/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$DIR"

kubectl get crd "$CRD_NAME" -o yaml > "$DIR/crd.yaml"
kubectl get "$CRD_NAME" -A -o json > "$DIR/deployconfigs.json"

count=$(jq '.items | length' "$DIR/deployconfigs.json")
with_status=$(jq '[.items[] | select(.status != null)] | length' "$DIR/deployconfigs.json")
echo "Saved CRD and $count DeployConfigs ($with_status with status) to $DIR"
