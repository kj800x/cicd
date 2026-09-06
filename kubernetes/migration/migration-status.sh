#!/usr/bin/env bash
# Show legacy and parameters fields side by side for every DeployConfig, and
# exit non-zero if any config is inconsistent.
#
# Usage: migration-status.sh
# Env:   CRD_NAME (default deployconfigs.cicd.coolkev.com)
#
# A config is inconsistent when it has a legacy field without the matching
# parameters.SHA entry, or the two disagree. The gate before deploying the
# new-only controller is: this script prints "0 inconsistent".
set -euo pipefail

CRD_NAME="${CRD_NAME:-deployconfigs.cicd.coolkev.com}"
json=$(kubectl get "$CRD_NAME" -A -o json)

printf 'NAMESPACE\tNAME\tLEGACY_REPO\tPARAM_REPO\tLEGACY_SHA\tPARAM_SHA\tSTATE\n'
jq -r '
  .items[]
  | . as $dc
  | (
      ($dc.spec.artifact != null and $dc.spec.parameters.SHA == null) or
      ($dc.status.artifact != null and $dc.status.parameters.SHA == null) or
      ($dc.spec.artifact != null and $dc.spec.parameters.SHA != null and
        ($dc.spec.artifact.repo != $dc.spec.parameters.SHA.repo or
         $dc.spec.artifact.owner != $dc.spec.parameters.SHA.owner)) or
      ($dc.status.artifact != null and $dc.status.parameters.SHA != null and
        $dc.status.artifact.sha != $dc.status.parameters.SHA.value)
    ) as $bad
  | [ .metadata.namespace, .metadata.name,
      (.spec.artifact.repo // "-"), (.spec.parameters.SHA.repo // "-"),
      ((.status.artifact.sha // "-") | .[0:12]), ((.status.parameters.SHA.value // "-") | .[0:12]),
      (if $bad then "INCONSISTENT" else "ok" end) ]
  | @tsv' <<<"$json"

bad=$(jq '[.items[] | select(
      (.spec.artifact != null and .spec.parameters.SHA == null) or
      (.status.artifact != null and .status.parameters.SHA == null) or
      (.spec.artifact != null and .spec.parameters.SHA != null and
        (.spec.artifact.repo != .spec.parameters.SHA.repo or .spec.artifact.owner != .spec.parameters.SHA.owner)) or
      (.status.artifact != null and .status.parameters.SHA != null and
        .status.artifact.sha != .status.parameters.SHA.value)
    )] | length' <<<"$json")
echo
echo "$bad inconsistent"
[[ "$bad" -eq 0 ]]
