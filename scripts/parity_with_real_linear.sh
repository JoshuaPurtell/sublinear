#!/usr/bin/env bash
set -euo pipefail

PORT="${SUBLINEAR_PARITY_PORT:-9899}"
SUB_AUTH_KEY="${SUBLINEAR_PARITY_KEY:-dev-token}"
SUB_DB_FILE="${SUBLINEAR_PARITY_DB:-/tmp/sublinear-real-parity-$$.db}"
SUB_BASE_URL="http://127.0.0.1:${PORT}"
SUB_GRAPHQL_URL="${SUB_BASE_URL}/graphql"
REAL_GRAPHQL_URL="${REAL_LINEAR_GRAPHQL_URL:-https://api.linear.app/graphql}"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -f "${SUB_DB_FILE}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [[ -z "${REAL_LINEAR_API_KEY:-}" && -z "${LINEAR_API_KEY:-}" && -z "${SB_LINEAR_API_KEY:-}" ]]; then
  if [[ -f "/Users/joshpurtell/Documents/Github/synth-background/.env" ]]; then
    # shellcheck disable=SC1091
    source "/Users/joshpurtell/Documents/Github/synth-background/.env"
  fi
fi

REAL_AUTH="${REAL_LINEAR_API_KEY:-${LINEAR_API_KEY:-${SB_LINEAR_API_KEY:-}}}"
if [[ -z "${REAL_AUTH}" ]]; then
  echo "Missing REAL Linear auth token. Set REAL_LINEAR_API_KEY (or LINEAR_API_KEY / SB_LINEAR_API_KEY)." >&2
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

shape_of() {
  local json="$1"
  jq -c '
    def shape:
      if type == "object" then
        (to_entries | sort_by(.key) | map({(.key): (.value | shape)}) | add)
      elif type == "array" then
        if length == 0 then ["<empty-array>"] else (map(shape) | unique) end
      else
        type
      end;
    .data | shape
  ' <<<"${json}"
}

has_errors() {
  local json="$1"
  jq -e '.errors and (.errors | length > 0)' >/dev/null <<<"${json}"
}

gql_call_any_status() {
  local endpoint="$1"
  local auth="$2"
  local query="$3"
  local vars="${4-}"
  local payload
  if [[ -z "${vars}" ]]; then
    vars='{}'
  fi
  payload="$(jq -cn --arg q "${query}" --arg v "${vars}" '{query:$q, variables:($v|fromjson)}')"
  curl -sS \
    -H "Authorization: ${auth}" \
    -H "Content-Type: application/json" \
    -d "${payload}" \
    "${endpoint}"
}

FAILS=0
CHECKS=0

assert_equal_values() {
  local label="$1"
  local sub_val="$2"
  local real_val="$3"
  CHECKS=$((CHECKS + 1))
  if [[ "${sub_val}" != "${real_val}" ]]; then
    echo "FAIL ${label}: value mismatch" >&2
    echo "  sublinear: ${sub_val}" >&2
    echo "  real     : ${real_val}" >&2
    FAILS=$((FAILS + 1))
  else
    echo "PASS ${label}"
  fi
}

compare_pair() {
  local label="$1"
  local query="$2"
  local vars_sub="${3-}"
  local vars_real="${4-}"

  local sub_resp real_resp
  sub_resp="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" "${query}" "${vars_sub}")"
  real_resp="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" "${query}" "${vars_real}")"
  CHECKS=$((CHECKS + 1))

  if has_errors "${sub_resp}"; then
    echo "FAIL ${label}: sublinear returned errors: $(jq -c '.errors' <<<"${sub_resp}")" >&2
    FAILS=$((FAILS + 1))
    return
  fi
  if has_errors "${real_resp}"; then
    echo "FAIL ${label}: real Linear returned errors: $(jq -c '.errors' <<<"${real_resp}")" >&2
    FAILS=$((FAILS + 1))
    return
  fi

  local sub_shape real_shape
  sub_shape="$(shape_of "${sub_resp}")"
  real_shape="$(shape_of "${real_resp}")"

  if [[ "${sub_shape}" != "${real_shape}" ]]; then
    echo "FAIL ${label}: shape mismatch" >&2
    echo "  sublinear: ${sub_shape}" >&2
    echo "  real     : ${real_shape}" >&2
    FAILS=$((FAILS + 1))
  else
    echo "PASS ${label}"
  fi
}

compare_value_pair() {
  local label="$1"
  local query="$2"
  local vars_sub="${3-}"
  local vars_real="${4-}"
  local jq_expr="$5"

  local sub_resp real_resp sub_val real_val
  sub_resp="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" "${query}" "${vars_sub}")"
  real_resp="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" "${query}" "${vars_real}")"

  if has_errors "${sub_resp}"; then
    CHECKS=$((CHECKS + 1))
    echo "FAIL ${label}: sublinear returned errors: $(jq -c '.errors' <<<"${sub_resp}")" >&2
    FAILS=$((FAILS + 1))
    return
  fi
  if has_errors "${real_resp}"; then
    CHECKS=$((CHECKS + 1))
    echo "FAIL ${label}: real Linear returned errors: $(jq -c '.errors' <<<"${real_resp}")" >&2
    FAILS=$((FAILS + 1))
    return
  fi

  sub_val="$(jq -c "${jq_expr}" <<<"${sub_resp}")"
  real_val="$(jq -c "${jq_expr}" <<<"${real_resp}")"
  assert_equal_values "${label}" "${sub_val}" "${real_val}"
}

echo "Starting sublinear..."
(
  cd "$(dirname "$0")/.."
  SUBLINEAR_PORT="${PORT}" \
  SUBLINEAR_BASE_URL="${SUB_BASE_URL}" \
  SUBLINEAR_API_KEY="${SUB_AUTH_KEY}" \
  SUBLINEAR_REQUIRE_AUTH=true \
  TURSO_DATABASE_URL="${SUB_DB_FILE}" \
  cargo run >/tmp/sublinear-real-parity.log 2>&1
) &
SERVER_PID=$!

for _ in {1..60}; do
  if curl -fsS "${SUB_BASE_URL}/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl -fsS "${SUB_BASE_URL}/healthz" >/dev/null

echo "Discovering team context..."
q_teams='query{teams{nodes{id name key}}}'
sub_teams="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" "${q_teams}" '{}')"
real_teams="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" "${q_teams}" '{}')"

if has_errors "${sub_teams}" || has_errors "${real_teams}"; then
  echo "Unable to list teams from one or both endpoints." >&2
  [[ $(has_errors "${sub_teams}"; echo $?) -eq 0 ]] && echo "sublinear errors: $(jq -c '.errors' <<<"${sub_teams}")" >&2
  [[ $(has_errors "${real_teams}"; echo $?) -eq 0 ]] && echo "real errors: $(jq -c '.errors' <<<"${real_teams}")" >&2
  exit 1
fi

sub_team_name="$(jq -r '.data.teams.nodes[0].name // empty' <<<"${sub_teams}")"
sub_team_id="$(jq -r '.data.teams.nodes[0].id // empty' <<<"${sub_teams}")"
sub_team_key="$(jq -r '.data.teams.nodes[0].key // empty' <<<"${sub_teams}")"

real_team_name="${REAL_LINEAR_TEAM_NAME:-}"
if [[ -n "${real_team_name}" ]]; then
  real_team_row="$(jq -c '.data.teams.nodes[] | select(.name=="'"${real_team_name}"'")' <<<"${real_teams}" | head -n 1 || true)"
  if [[ -z "${real_team_row}" ]]; then
    real_team_name=""
  fi
fi
if [[ -z "${real_team_name}" ]]; then
  real_team_name="$(jq -r '.data.teams.nodes[0].name // empty' <<<"${real_teams}")"
fi

real_team_id="$(jq -r '.data.teams.nodes[] | select(.name=="'"${real_team_name}"'") | .id' <<<"${real_teams}" | head -n 1)"
real_team_key="$(jq -r '.data.teams.nodes[] | select(.name=="'"${real_team_name}"'") | .key' <<<"${real_teams}" | head -n 1)"

if [[ -z "${sub_team_id}" || -z "${real_team_id}" ]]; then
  echo "Could not resolve team IDs for both environments." >&2
  exit 1
fi

project_name="sublinear-parity-$(date +%Y%m%d%H%M%S)"

echo "Running side-by-side shape comparisons..."

compare_pair "viewer" 'query{viewer{id name email}}'
compare_pair "teams.list" 'query{teams{nodes{id name key}}}'
compare_pair "teams.by_name" \
  'query($name:String!){teams(filter:{name:{eq:$name}}){nodes{id name key}}}' \
  "{\"name\":\"${sub_team_name}\"}" \
  "{\"name\":\"${real_team_name}\"}"

compare_pair "project.create" \
  'mutation($teamId:String!,$name:String!){projectCreate(input:{teamIds:[$teamId],name:$name}){success project{id name url slugId state archivedAt}}}' \
  "{\"teamId\":\"${sub_team_id}\",\"name\":\"${project_name}\"}" \
  "{\"teamId\":\"${real_team_id}\",\"name\":\"${project_name}\"}"

sub_project="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" 'query($name:String!){projects(filter:{name:{eq:$name}},first:1){nodes{id name slugId state archivedAt url}}}' "{\"name\":\"${project_name}\"}")"
real_project="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" 'query($name:String!){projects(filter:{name:{eq:$name}},first:1){nodes{id name slugId state archivedAt url}}}' "{\"name\":\"${project_name}\"}")"
compare_pair "project.by_name" \
  'query($name:String!){projects(filter:{name:{eq:$name}},first:1){nodes{id name slugId state archivedAt url}}}' \
  "{\"name\":\"${project_name}\"}" \
  "{\"name\":\"${project_name}\"}"

random_missing_name="missing-project-${RANDOM}-$(date +%s)"
compare_value_pair "project.by_name.missing_count" \
  'query($name:String!){projects(filter:{name:{eq:$name}},first:1){nodes{id}}}' \
  "{\"name\":\"${random_missing_name}\"}" \
  "{\"name\":\"${random_missing_name}\"}" \
  '.data.projects.nodes | length'
sub_project_id="$(jq -r '.data.projects.nodes[0].id // empty' <<<"${sub_project}")"
real_project_id="$(jq -r '.data.projects.nodes[0].id // empty' <<<"${real_project}")"
if [[ -z "${sub_project_id}" || -z "${real_project_id}" ]]; then
  echo "Could not resolve created project IDs." >&2
  exit 1
fi

compare_pair "issue.create" \
  'mutation($teamId:String!,$projectId:String,$title:String!,$description:String){issueCreate(input:{teamId:$teamId,projectId:$projectId,title:$title,description:$description}){success issue{id identifier title url state{id name type}}}}' \
  "{\"teamId\":\"${sub_team_id}\",\"projectId\":\"${sub_project_id}\",\"title\":\"parity issue\",\"description\":\"parity description\"}" \
  "{\"teamId\":\"${real_team_id}\",\"projectId\":\"${real_project_id}\",\"title\":\"parity issue\",\"description\":\"parity description\"}"

sub_issue_create="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" 'mutation($teamId:String!,$projectId:String,$title:String!,$description:String){issueCreate(input:{teamId:$teamId,projectId:$projectId,title:$title,description:$description}){success issue{id identifier title url state{id name type}}}}' "{\"teamId\":\"${sub_team_id}\",\"projectId\":\"${sub_project_id}\",\"title\":\"parity issue 2\",\"description\":\"parity description\"}")"
real_issue_create="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" 'mutation($teamId:String!,$projectId:String,$title:String!,$description:String){issueCreate(input:{teamId:$teamId,projectId:$projectId,title:$title,description:$description}){success issue{id identifier title url state{id name type}}}}' "{\"teamId\":\"${real_team_id}\",\"projectId\":\"${real_project_id}\",\"title\":\"parity issue 2\",\"description\":\"parity description\"}")"
sub_issue_id="$(jq -r '.data.issueCreate.issue.id // empty' <<<"${sub_issue_create}")"
real_issue_id="$(jq -r '.data.issueCreate.issue.id // empty' <<<"${real_issue_create}")"
sub_identifier="$(jq -r '.data.issueCreate.issue.identifier // empty' <<<"${sub_issue_create}")"
real_identifier="$(jq -r '.data.issueCreate.issue.identifier // empty' <<<"${real_issue_create}")"

compare_pair "issue.get" \
  'query($id:String!){issue(id:$id){id identifier title url description assignee{id name email} project{id name slugId state archivedAt} state{name type} labels{nodes{name}} updatedAt}}' \
  "{\"id\":\"${sub_issue_id}\"}" \
  "{\"id\":\"${real_issue_id}\"}"

compare_value_pair "project.issues.first_1_count" \
  'query($projectId:String!,$first:Int!){project(id:$projectId){issues(first:$first){nodes{id}}}}' \
  "{\"projectId\":\"${sub_project_id}\",\"first\":1}" \
  "{\"projectId\":\"${real_project_id}\",\"first\":1}" \
  '.data.project.issues.nodes | length'

compare_pair "workflowStates.top_level" \
  'query($teamId:ID!){workflowStates(filter:{team:{id:{eq:$teamId}}}){nodes{id name type}}}' \
  "{\"teamId\":\"${sub_team_id}\"}" \
  "{\"teamId\":\"${real_team_id}\"}"

sub_states="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" 'query($teamId:ID!){workflowStates(filter:{team:{id:{eq:$teamId}}}){nodes{id name type}}}' "{\"teamId\":\"${sub_team_id}\"}")"
real_states="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" 'query($teamId:ID!){workflowStates(filter:{team:{id:{eq:$teamId}}}){nodes{id name type}}}' "{\"teamId\":\"${real_team_id}\"}")"
sub_in_progress="$(jq -r '.data.workflowStates.nodes[] | select(.name=="In Progress" or .type=="started") | .id' <<<"${sub_states}" | head -n 1)"
real_in_progress="$(jq -r '.data.workflowStates.nodes[] | select(.name=="In Progress" or .type=="started") | .id' <<<"${real_states}" | head -n 1)"
sub_done="$(jq -r '.data.workflowStates.nodes[] | select(.name=="Done" or .type=="completed") | .id' <<<"${sub_states}" | head -n 1)"
real_done="$(jq -r '.data.workflowStates.nodes[] | select(.name=="Done" or .type=="completed") | .id' <<<"${real_states}" | head -n 1)"

compare_pair "issueUpdate.state_only" \
  'mutation($id:String!,$stateId:String!){issueUpdate(id:$id,input:{stateId:$stateId}){success}}' \
  "{\"id\":\"${sub_issue_id}\",\"stateId\":\"${sub_in_progress}\"}" \
  "{\"id\":\"${real_issue_id}\",\"stateId\":\"${real_in_progress}\"}"

roundtrip_desc="roundtrip-desc-$(date +%s)"
compare_pair "issueUpdate.description_roundtrip" \
  'mutation($id:String!,$input:IssueUpdateInput!){issueUpdate(id:$id,input:$input){success issue{id title url state{id name type}}}}' \
  "{\"id\":\"${sub_issue_id}\",\"input\":{\"description\":\"${roundtrip_desc}\"}}" \
  "{\"id\":\"${real_issue_id}\",\"input\":{\"description\":\"${roundtrip_desc}\"}}"
compare_value_pair "issue.description_matches" \
  'query($id:String!){issue(id:$id){description}}' \
  "{\"id\":\"${sub_issue_id}\"}" \
  "{\"id\":\"${real_issue_id}\"}" \
  '.data.issue.description'

compare_pair "issues.list.project_scope" \
  'query($teamId:ID!,$projectId:ID!,$first:Int!){issues(filter:{team:{id:{eq:$teamId}},project:{id:{eq:$projectId}},state:{name:{neq:"Backlog"}}},first:$first,orderBy:updatedAt){nodes{id identifier title url description assignee{id name email} project{id name slugId state archivedAt} state{name type} labels{nodes{name}} updatedAt}}}' \
  "{\"teamId\":\"${sub_team_id}\",\"projectId\":\"${sub_project_id}\",\"first\":25}" \
  "{\"teamId\":\"${real_team_id}\",\"projectId\":\"${real_project_id}\",\"first\":25}"

compare_pair "issues.list.team_scope" \
  'query($teamId:ID!,$first:Int!){issues(filter:{team:{id:{eq:$teamId}},state:{name:{neq:"Backlog"}}},first:$first,orderBy:updatedAt){nodes{id identifier title}}}' \
  "{\"teamId\":\"${sub_team_id}\",\"first\":25}" \
  "{\"teamId\":\"${real_team_id}\",\"first\":25}"

compare_pair "commentCreate.success_only" \
  'mutation($issueId:String!,$body:String!){commentCreate(input:{issueId:$issueId,body:$body}){success}}' \
  "{\"issueId\":\"${sub_issue_id}\",\"body\":\"parity comment sb\"}" \
  "{\"issueId\":\"${real_issue_id}\",\"body\":\"parity comment sb\"}"

multiline_body=$'parity multiline comment\nline-two'
compare_pair "commentCreate.multiline_body" \
  'mutation($issueId:String!,$body:String!){commentCreate(input:{issueId:$issueId,body:$body}){success comment{id body url}}}' \
  "$(jq -cn --arg issueId "${sub_issue_id}" --arg body "${multiline_body}" '{issueId:$issueId, body:$body}')" \
  "$(jq -cn --arg issueId "${real_issue_id}" --arg body "${multiline_body}" '{issueId:$issueId, body:$body}')"

compare_pair "team.states_nested" \
  'query($teamId:String!){team(id:$teamId){states{nodes{id name type}}}}' \
  "{\"teamId\":\"${sub_team_id}\"}" \
  "{\"teamId\":\"${real_team_id}\"}"

compare_pair "issueUpdate.with_issue_payload" \
  'mutation($id:String!,$input:IssueUpdateInput!){issueUpdate(id:$id,input:$input){success issue{id identifier title url state{id name type}}}}' \
  "{\"id\":\"${sub_issue_id}\",\"input\":{\"stateId\":\"${sub_done}\",\"title\":\"smr parity updated\"}}" \
  "{\"id\":\"${real_issue_id}\",\"input\":{\"stateId\":\"${real_done}\",\"title\":\"smr parity updated\"}}"

compare_pair "project.issues" \
  'query($projectId:String!,$first:Int!){project(id:$projectId){issues(first:$first){nodes{id identifier title url state{id name type}}}}}' \
  "{\"projectId\":\"${sub_project_id}\",\"first\":25}" \
  "{\"projectId\":\"${real_project_id}\",\"first\":25}"

compare_pair "commentCreate.with_comment_payload" \
  'mutation($issueId:String!,$body:String!){commentCreate(input:{issueId:$issueId,body:$body}){success comment{id body url}}}' \
  "{\"issueId\":\"${sub_issue_id}\",\"body\":\"smr parity comment\"}" \
  "{\"issueId\":\"${real_issue_id}\",\"body\":\"smr parity comment\"}"

sub_num="${sub_identifier##*-}"
real_num="${real_identifier##*-}"
compare_pair "issues.by_identifiers_path" \
  'query($teamKey:String!,$numbers:[Float!]!){issues(filter:{team:{key:{eq:$teamKey}},number:{in:$numbers}},first:50){nodes{identifier title description url state{name type} labels{nodes{name}} project{id name}}}}' \
  "{\"teamKey\":\"${sub_team_key}\",\"numbers\":[${sub_num}]}" \
  "{\"teamKey\":\"${real_team_key}\",\"numbers\":[${real_num}]}"

compare_pair "issues.by_identifiers_with_unknown" \
  'query($teamKey:String!,$numbers:[Float!]!){issues(filter:{team:{key:{eq:$teamKey}},number:{in:$numbers}},first:50){nodes{identifier title}}}' \
  "{\"teamKey\":\"${sub_team_key}\",\"numbers\":[${sub_num},99999999]}" \
  "{\"teamKey\":\"${real_team_key}\",\"numbers\":[${real_num},99999999]}"

compare_pair "issueArchive" \
  'mutation($id:String!){issueArchive(id:$id){success}}' \
  "{\"id\":\"${sub_issue_id}\"}" \
  "{\"id\":\"${real_issue_id}\"}"

sub_after_archive="$(gql_call "${SUB_GRAPHQL_URL}" "${SUB_AUTH_KEY}" 'query($projectId:String!,$first:Int!){project(id:$projectId){issues(first:$first){nodes{id}}}}' "{\"projectId\":\"${sub_project_id}\",\"first\":200}")"
real_after_archive="$(gql_call "${REAL_GRAPHQL_URL}" "${REAL_AUTH}" 'query($projectId:String!,$first:Int!){project(id:$projectId){issues(first:$first){nodes{id}}}}' "{\"projectId\":\"${real_project_id}\",\"first\":200}")"
sub_contains_archived="$(jq -r '.data.project.issues.nodes | map(.id) | index("'"${sub_issue_id}"'") != null' <<<"${sub_after_archive}")"
real_contains_archived="$(jq -r '.data.project.issues.nodes | map(.id) | index("'"${real_issue_id}"'") != null' <<<"${real_after_archive}")"
assert_equal_values "issueArchive.visibility_in_project_list" "${sub_contains_archived}" "${real_contains_archived}"

bad_token_query='query{viewer{id}}'
sub_bad_auth_resp="$(gql_call_any_status "${SUB_GRAPHQL_URL}" "invalid-token" "${bad_token_query}" '{}')"
real_bad_auth_resp="$(gql_call_any_status "${REAL_GRAPHQL_URL}" "invalid-token" "${bad_token_query}" '{}')"
sub_bad_has_errors="false"
real_bad_has_errors="false"
has_errors "${sub_bad_auth_resp}" && sub_bad_has_errors="true" || true
has_errors "${real_bad_auth_resp}" && real_bad_has_errors="true" || true
assert_equal_values "auth.invalid_token_error_mode" "${sub_bad_has_errors}" "${real_bad_has_errors}"

compare_value_pair "team.states.non_empty_bool" \
  'query($teamId:String!){team(id:$teamId){states{nodes{id}}}}' \
  "{\"teamId\":\"${sub_team_id}\"}" \
  "{\"teamId\":\"${real_team_id}\"}" \
  '.data.team.states.nodes | length > 0'

echo
if [[ ${FAILS} -gt 0 ]]; then
  echo "Parity compare complete: ${FAILS}/${CHECKS} checks failed."
  exit 1
fi
echo "Parity compare complete: all ${CHECKS} checks passed."
