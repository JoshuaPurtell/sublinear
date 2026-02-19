#!/usr/bin/env bash
set -euo pipefail

PORT="${SUBLINEAR_PARITY_PORT:-9898}"
AUTH_KEY="${SUBLINEAR_PARITY_KEY:-dev-token}"
DB_FILE="${SUBLINEAR_PARITY_DB:-/tmp/sublinear-parity-$$.db}"
BASE_URL="http://127.0.0.1:${PORT}"
GRAPHQL_URL="${BASE_URL}/graphql"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -f "${DB_FILE}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

gql() {
  local query="$1"
  local vars="${2-}"
  if [[ -z "${vars}" ]]; then
    vars='{}'
  fi
  local payload
  if ! payload="$(jq -cn --arg q "$query" --arg v "$vars" '{query:$q, variables:($v|fromjson)}')"; then
    echo "Invalid vars JSON: ${vars}" >&2
    exit 1
  fi
  curl -fsS \
    -H "Authorization: ${AUTH_KEY}" \
    -H "Content-Type: application/json" \
    -d "${payload}" \
    "${GRAPHQL_URL}"
}

assert_no_errors() {
  local resp="$1"
  local label="$2"
  if jq -e '.errors and (.errors | length > 0)' >/dev/null <<<"${resp}"; then
    echo "FAIL ${label}: $(jq -c '.errors' <<<"${resp}")" >&2
    exit 1
  fi
}

echo "Starting sublinear for parity tests..."
(
  cd "$(dirname "$0")/.."
  SUBLINEAR_PORT="${PORT}" \
  SUBLINEAR_BASE_URL="${BASE_URL}" \
  SUBLINEAR_API_KEY="${AUTH_KEY}" \
  SUBLINEAR_REQUIRE_AUTH=true \
  TURSO_DATABASE_URL="${DB_FILE}" \
  cargo run >/tmp/sublinear-parity.log 2>&1
) &
SERVER_PID=$!

for _ in {1..60}; do
  if curl -fsS "${BASE_URL}/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl -fsS "${BASE_URL}/healthz" >/dev/null

echo "Running synth-background parity queries..."

resp="$(gql 'query{viewer{id name email}}')"
assert_no_errors "${resp}" "viewer"
jq -e '.data.viewer.id and .data.viewer.name and .data.viewer.email' >/dev/null <<<"${resp}"

resp="$(gql 'query($name:String!){teams(filter:{name:{eq:$name}}){nodes{id name key}}}' '{"name":"Synth"}')"
assert_no_errors "${resp}" "team_by_name"
team_id="$(jq -r '.data.teams.nodes[0].id' <<<"${resp}")"
team_key="$(jq -r '.data.teams.nodes[0].key' <<<"${resp}")"

resp="$(gql 'mutation($teamId:String!,$name:String!){projectCreate(input:{teamIds:[$teamId],name:$name}){success project{id name url slugId state archivedAt}}}' "{\"teamId\":\"${team_id}\",\"name\":\"parity-project\"}")"
assert_no_errors "${resp}" "project_create"
project_id="$(jq -r '.data.projectCreate.project.id' <<<"${resp}")"

resp="$(gql 'query($name:String!){projects(filter:{name:{eq:$name}},first:1){nodes{id name slugId state archivedAt url}}}' '{"name":"parity-project"}')"
assert_no_errors "${resp}" "project_by_name"
jq -e '.data.projects.nodes[0].id == "'"${project_id}"'"' >/dev/null <<<"${resp}"

resp="$(gql 'mutation($teamId:String!,$projectId:String,$title:String!,$description:String){issueCreate(input:{teamId:$teamId,projectId:$projectId,title:$title,description:$description}){success issue{id identifier title url state{id name type}}}}' "{\"teamId\":\"${team_id}\",\"projectId\":\"${project_id}\",\"title\":\"parity issue\",\"description\":\"parity description\"}")"
assert_no_errors "${resp}" "issue_create"
issue_id="$(jq -r '.data.issueCreate.issue.id' <<<"${resp}")"
identifier="$(jq -r '.data.issueCreate.issue.identifier' <<<"${resp}")"

resp="$(gql 'query($id:String!){issue(id:$id){id identifier title url description assignee{id name email} project{id name slugId state archivedAt} state{name type} labels{nodes{name}} updatedAt}}' "{\"id\":\"${issue_id}\"}")"
assert_no_errors "${resp}" "get_issue"
jq -e '.data.issue.identifier == "'"${identifier}"'"' >/dev/null <<<"${resp}"

resp="$(gql 'query($teamId:ID!){workflowStates(filter:{team:{id:{eq:$teamId}}}){nodes{id name type}}}' "{\"teamId\":\"${team_id}\"}")"
assert_no_errors "${resp}" "workflow_states"
in_progress_state_id="$(jq -r '.data.workflowStates.nodes[] | select(.name=="In Progress") | .id' <<<"${resp}" | head -n 1)"
done_state_id="$(jq -r '.data.workflowStates.nodes[] | select(.name=="Done") | .id' <<<"${resp}" | head -n 1)"

resp="$(gql 'mutation($id:String!,$stateId:String!){issueUpdate(id:$id,input:{stateId:$stateId}){success}}' "{\"id\":\"${issue_id}\",\"stateId\":\"${in_progress_state_id}\"}")"
assert_no_errors "${resp}" "update_issue_state"
jq -e '.data.issueUpdate.success == true' >/dev/null <<<"${resp}"

resp="$(gql 'query($teamId:ID!,$projectId:ID!,$first:Int!){issues(filter:{team:{id:{eq:$teamId}},project:{id:{eq:$projectId}},state:{name:{neq:"Backlog"}}},first:$first,orderBy:updatedAt){nodes{id identifier title url description assignee{id name email} project{id name slugId state archivedAt} state{name type} labels{nodes{name}} updatedAt}}}' "{\"teamId\":\"${team_id}\",\"projectId\":\"${project_id}\",\"first\":25}")"
assert_no_errors "${resp}" "list_ready_issues_with_project"
jq -e '.data.issues.nodes[] | select(.id=="'"${issue_id}"'")' >/dev/null <<<"${resp}"

resp="$(gql 'query($teamId:ID!,$first:Int!){issues(filter:{team:{id:{eq:$teamId}},state:{name:{neq:"Backlog"}}},first:$first,orderBy:updatedAt){nodes{id identifier title}}}' "{\"teamId\":\"${team_id}\",\"first\":25}")"
assert_no_errors "${resp}" "list_ready_issues_no_project"
jq -e '.data.issues.nodes[] | select(.id=="'"${issue_id}"'")' >/dev/null <<<"${resp}"

resp="$(gql 'mutation($issueId:String!,$body:String!){commentCreate(input:{issueId:$issueId,body:$body}){success}}' "{\"issueId\":\"${issue_id}\",\"body\":\"dispatch comment\"}")"
assert_no_errors "${resp}" "create_comment_sb"
jq -e '.data.commentCreate.success == true' >/dev/null <<<"${resp}"

resp="$(gql 'mutation($id:String!,$labelId:String!){issueAddLabel(id:$id,labelId:$labelId){success}}' "{\"id\":\"${issue_id}\",\"labelId\":\"agent-eligible\"}")"
assert_no_errors "${resp}" "add_label"
jq -e '.data.issueAddLabel.success == true' >/dev/null <<<"${resp}"

issue_number="${identifier##*-}"
resp="$(gql 'query($teamKey:String!,$numbers:[Float!]!){issues(filter:{team:{key:{eq:$teamKey}},number:{in:$numbers}},first:50){nodes{identifier title description url state{name type} labels{nodes{name}} project{id name}}}}' "{\"teamKey\":\"${team_key}\",\"numbers\":[${issue_number}]}" )"
assert_no_errors "${resp}" "issues_by_identifiers"
jq -e '.data.issues.nodes[] | select(.identifier=="'"${identifier}"'")' >/dev/null <<<"${resp}"

echo "Running smr parity queries..."

resp="$(gql 'query{teams{nodes{id name key}}}')"
assert_no_errors "${resp}" "smr_list_teams"

resp="$(gql 'mutation($issueId:String!,$body:String!){commentCreate(input:{issueId:$issueId,body:$body}){success comment{id body url}}}' "{\"issueId\":\"${issue_id}\",\"body\":\"smr comment\"}")"
assert_no_errors "${resp}" "smr_create_comment"
jq -e '.data.commentCreate.comment.id and .data.commentCreate.comment.url' >/dev/null <<<"${resp}"

resp="$(gql 'query($teamId:String!){team(id:$teamId){states{nodes{id name type}}}}' "{\"teamId\":\"${team_id}\"}")"
assert_no_errors "${resp}" "smr_list_workflow_states"
jq -e '.data.team.states.nodes | length > 0' >/dev/null <<<"${resp}"

resp="$(gql 'mutation($id:String!,$input:IssueUpdateInput!){issueUpdate(id:$id,input:$input){success issue{id identifier title url state{id name type}}}}' "{\"id\":\"${issue_id}\",\"input\":{\"stateId\":\"${done_state_id}\",\"title\":\"smr updated\"}}")"
assert_no_errors "${resp}" "smr_update_issue"
jq -e '.data.issueUpdate.success == true' >/dev/null <<<"${resp}"

resp="$(gql 'query($projectId:String!,$first:Int!){project(id:$projectId){issues(first:$first){nodes{id identifier title url state{id name type}}}}}' "{\"projectId\":\"${project_id}\",\"first\":25}")"
assert_no_errors "${resp}" "smr_list_project_issues"
jq -e '.data.project.issues.nodes[] | select(.id=="'"${issue_id}"'")' >/dev/null <<<"${resp}"

resp="$(gql 'mutation($id:String!){issueArchive(id:$id){success}}' "{\"id\":\"${issue_id}\"}")"
assert_no_errors "${resp}" "smr_archive_issue"
jq -e '.data.issueArchive.success == true' >/dev/null <<<"${resp}"

echo "PASS: parity tests succeeded."
