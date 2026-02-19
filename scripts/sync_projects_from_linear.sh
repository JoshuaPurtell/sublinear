#!/usr/bin/env bash
set -euo pipefail

SUBLINEAR_GRAPHQL_URL="${SUBLINEAR_GRAPHQL_URL:-http://127.0.0.1:8787/graphql}"
SUBLINEAR_API_KEY="${SUBLINEAR_API_KEY:-dev-token}"
REAL_LINEAR_GRAPHQL_URL="${REAL_LINEAR_GRAPHQL_URL:-https://api.linear.app/graphql}"
PAGE_SIZE="${SYNC_PAGE_SIZE:-100}"

if [[ -z "${REAL_LINEAR_API_KEY:-}" && -z "${LINEAR_API_KEY:-}" && -f ".env.real-linear" ]]; then
  # shellcheck disable=SC1091
  source ".env.real-linear"
fi

REAL_AUTH="${REAL_LINEAR_API_KEY:-${LINEAR_API_KEY:-}}"
if [[ -z "${REAL_AUTH}" ]]; then
  echo "Missing real Linear API key. Set REAL_LINEAR_API_KEY (or LINEAR_API_KEY)." >&2
  exit 1
fi

gql_call() {
  local endpoint="$1"
  local auth="$2"
  local query="$3"
  local vars="${4-}"
  local payload
  if [[ -z "${vars}" ]]; then
    vars='{}'
  fi
  payload="$(jq -cn --arg q "${query}" --arg v "${vars}" '{query:$q, variables:($v|fromjson)}')"
  curl -fsS \
    -H "Authorization: ${auth}" \
    -H "Content-Type: application/json" \
    -d "${payload}" \
    "${endpoint}"
}

has_errors() {
  local json="$1"
  jq -e '.errors and (.errors | length > 0)' >/dev/null <<<"${json}"
}

fetch_query='query($first:Int!,$after:String){projects(first:$first,after:$after){nodes{id name slugId state archivedAt url} pageInfo{hasNextPage endCursor}}}'
import_mutation='mutation($input:AdminImportProjectInput!){adminImportProject(input:$input){success project{id name slugId state archivedAt url}}}'

after=""
total_seen=0
total_imported=0

echo "Syncing Linear projects into sublinear..."
echo "  sublinear: ${SUBLINEAR_GRAPHQL_URL}"
echo "  real API : ${REAL_LINEAR_GRAPHQL_URL}"

while true; do
  vars="$(jq -cn --argjson first "${PAGE_SIZE}" --arg after "${after}" '{first:$first, after:(if $after == "" then null else $after end)}')"
  real_resp="$(gql_call "${REAL_LINEAR_GRAPHQL_URL}" "${REAL_AUTH}" "${fetch_query}" "${vars}")"

  if has_errors "${real_resp}"; then
    echo "Real Linear query failed: $(jq -c '.errors' <<<"${real_resp}")" >&2
    exit 1
  fi

  page_count="$(jq -r '.data.projects.nodes | length' <<<"${real_resp}")"
  if [[ "${page_count}" -eq 0 ]]; then
    break
  fi

  while IFS= read -r project_json; do
    input_vars="$(jq -cn --argjson p "${project_json}" '{input:$p}')"
    import_resp="$(gql_call "${SUBLINEAR_GRAPHQL_URL}" "${SUBLINEAR_API_KEY}" "${import_mutation}" "${input_vars}")"
    if has_errors "${import_resp}"; then
      echo "Sublinear import failed for project $(jq -r '.id' <<<"${project_json}"):" >&2
      echo "$(jq -c '.errors' <<<"${import_resp}")" >&2
      exit 1
    fi
    total_imported=$((total_imported + 1))
  done < <(jq -c '.data.projects.nodes[]' <<<"${real_resp}")

  total_seen=$((total_seen + page_count))
  has_next="$(jq -r '.data.projects.pageInfo.hasNextPage' <<<"${real_resp}")"
  after="$(jq -r '.data.projects.pageInfo.endCursor // ""' <<<"${real_resp}")"

  echo "  imported ${total_imported} projects so far..."
  if [[ "${has_next}" != "true" ]]; then
    break
  fi
done

echo "Done. Imported ${total_imported} projects from real Linear into sublinear."
