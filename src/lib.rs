use std::{env, net::SocketAddr, sync::Arc};

use anyhow::{Context as AnyhowContext, Result};
use async_graphql::http::{GraphQLPlaygroundConfig, playground_source};
use async_graphql::{
    ComplexObject, Context, EmptySubscription, Enum, Error, InputObject, Object, Schema,
    SimpleObject,
};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, header},
    response::{Html, IntoResponse},
    routing::get,
};
use chrono::Utc;
use libsql::{Builder, Connection, Value, de};
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

type AppSchema = Schema<QueryRoot, MutationRoot, EmptySubscription>;
type GqlResult<T> = std::result::Result<T, Error>;

#[derive(Clone)]
struct Config {
    port: u16,
    db_url: String,
    db_token: Option<String>,
    base_url: String,
    require_auth: bool,
    api_key: Option<String>,
    seed_viewer_name: String,
    seed_viewer_email: String,
    seed_team_name: String,
    seed_team_key: String,
}

impl Config {
    fn from_env() -> Self {
        let port = env::var("SUBLINEAR_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8787);
        let db_url = env::var("TURSO_DATABASE_URL").unwrap_or_else(|_| "sublinear.db".to_string());
        let db_token = env::var("TURSO_AUTH_TOKEN").ok().filter(|v| !v.is_empty());
        let base_url =
            env::var("SUBLINEAR_BASE_URL").unwrap_or_else(|_| format!("http://localhost:{port}"));
        let require_auth = env::var("SUBLINEAR_REQUIRE_AUTH")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(true);
        let api_key = env::var("SUBLINEAR_API_KEY").ok().filter(|v| !v.is_empty());
        let seed_viewer_name =
            env::var("SUBLINEAR_SEED_VIEWER_NAME").unwrap_or_else(|_| "Sublinear Dev".to_string());
        let seed_viewer_email = env::var("SUBLINEAR_SEED_VIEWER_EMAIL")
            .unwrap_or_else(|_| "sublinear@example.com".to_string());
        let seed_team_name =
            env::var("SUBLINEAR_SEED_TEAM_NAME").unwrap_or_else(|_| "Synth".to_string());
        let seed_team_key = env::var("SUBLINEAR_SEED_TEAM_KEY")
            .ok()
            .map(|v| sanitize_team_key(&v))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "SYN".to_string());

        Self {
            port,
            db_url,
            db_token,
            base_url,
            require_auth,
            api_key,
            seed_viewer_name,
            seed_viewer_email,
            seed_team_name,
            seed_team_key,
        }
    }
}

#[derive(Clone)]
struct AppContext {
    conn: Connection,
    base_url: String,
    require_auth: bool,
}

#[derive(Clone)]
struct AppState {
    schema: AppSchema,
    config: Arc<Config>,
}

#[derive(Clone, Copy)]
struct RequestAuth {
    authorized: bool,
}

pub async fn run_from_env() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sublinear=info".into()),
        )
        .init();

    let config = Arc::new(Config::from_env());
    let conn = open_connection(&config).await?;
    migrate(&conn).await?;
    seed_defaults(&conn, &config).await?;

    let schema = Schema::build(QueryRoot, MutationRoot, EmptySubscription)
        .data(Arc::new(AppContext {
            conn: conn.clone(),
            base_url: config.base_url.clone(),
            require_auth: config.require_auth,
        }))
        .finish();

    let app = Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/graphql", get(graphql_playground).post(graphql_handler))
        .with_state(AppState {
            schema,
            config: config.clone(),
        });

    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    info!(
        "sublinear listening on http://{} (NOT FOR PRODUCTION USE)",
        addr
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn root() -> impl IntoResponse {
    "sublinear: dev-only Linear API replacement (NOT FOR PRODUCTION USE)"
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

async fn graphql_playground() -> impl IntoResponse {
    Html(playground_source(GraphQLPlaygroundConfig::new("/graphql")))
}

async fn graphql_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let authorized = is_authorized(&headers, &state.config);
    state
        .schema
        .execute(req.into_inner().data(RequestAuth { authorized }))
        .await
        .into()
}

fn is_authorized(headers: &HeaderMap, cfg: &Config) -> bool {
    if !cfg.require_auth {
        return true;
    }
    let Some(raw) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = raw.to_str() else {
        return false;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Some(expected) = cfg.api_key.as_deref() {
        trimmed == expected || trimmed == format!("Bearer {expected}")
    } else {
        true
    }
}

fn ensure_auth(ctx: &Context<'_>) -> GqlResult<()> {
    let app = ctx.data_unchecked::<Arc<AppContext>>();
    if !app.require_auth {
        return Ok(());
    }
    let authorized = ctx
        .data_opt::<RequestAuth>()
        .map(|a| a.authorized)
        .unwrap_or(false);
    if authorized {
        Ok(())
    } else {
        Err(Error::new("Unauthorized"))
    }
}

fn app_ctx(ctx: &Context<'_>) -> Arc<AppContext> {
    ctx.data_unchecked::<Arc<AppContext>>().clone()
}

fn gql_error<E: std::fmt::Display>(err: E) -> Error {
    Error::new(err.to_string())
}

async fn open_connection(cfg: &Config) -> Result<Connection> {
    let db = if looks_remote_url(&cfg.db_url) {
        let token = cfg.db_token.clone().unwrap_or_default();
        Builder::new_remote(cfg.db_url.clone(), token)
            .build()
            .await
            .with_context(|| format!("failed to connect remote turso {}", cfg.db_url))?
    } else {
        let local_path = cfg.db_url.strip_prefix("file:").unwrap_or(&cfg.db_url);
        Builder::new_local(local_path)
            .build()
            .await
            .with_context(|| format!("failed to open local db {local_path}"))?
    };
    db.connect().context("failed to create db connection")
}

fn looks_remote_url(url: &str) -> bool {
    url.starts_with("libsql://") || url.starts_with("https://") || url.starts_with("http://")
}

async fn migrate(conn: &Connection) -> Result<()> {
    let stmts = [
        "PRAGMA foreign_keys = ON",
        "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT NOT NULL, email TEXT NOT NULL, created_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS teams (id TEXT PRIMARY KEY, name TEXT NOT NULL, key TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS team_members (team_id TEXT NOT NULL, user_id TEXT NOT NULL, PRIMARY KEY(team_id, user_id))",
        "CREATE TABLE IF NOT EXISTS workflow_states (id TEXT PRIMARY KEY, team_id TEXT NOT NULL, name TEXT NOT NULL, type TEXT NOT NULL, position INTEGER NOT NULL)",
        "CREATE TABLE IF NOT EXISTS projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, slug_id TEXT NOT NULL UNIQUE, state TEXT, archived_at TEXT, url TEXT NOT NULL, created_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS project_teams (project_id TEXT NOT NULL, team_id TEXT NOT NULL, PRIMARY KEY(project_id, team_id))",
        "CREATE TABLE IF NOT EXISTS issues (id TEXT PRIMARY KEY, team_id TEXT NOT NULL, project_id TEXT, number INTEGER NOT NULL, identifier TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT, state_id TEXT NOT NULL, assignee_id TEXT, archived INTEGER NOT NULL DEFAULT 0, url TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS labels (id TEXT PRIMARY KEY, name TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS issue_labels (issue_id TEXT NOT NULL, label_id TEXT NOT NULL, PRIMARY KEY(issue_id, label_id))",
        "CREATE TABLE IF NOT EXISTS comments (id TEXT PRIMARY KEY, issue_id TEXT NOT NULL, body TEXT NOT NULL, url TEXT NOT NULL, created_at TEXT NOT NULL)",
    ];
    for stmt in stmts {
        conn.execute(stmt, ()).await?;
    }
    Ok(())
}

async fn seed_defaults(conn: &Connection, cfg: &Config) -> Result<()> {
    let now = now_iso();
    let viewer_id = "viewer_default";
    let team_id = "team_default";

    if count(conn, "SELECT COUNT(*) as value FROM users", vec![]).await? == 0 {
        conn.execute(
            "INSERT INTO users (id, name, email, created_at) VALUES (?1, ?2, ?3, ?4)",
            vals(vec![
                viewer_id.into(),
                cfg.seed_viewer_name.clone().into(),
                cfg.seed_viewer_email.clone().into(),
                now.clone().into(),
            ]),
        )
        .await?;
    }

    if count(conn, "SELECT COUNT(*) as value FROM teams", vec![]).await? == 0 {
        conn.execute(
            "INSERT INTO teams (id, name, key, created_at) VALUES (?1, ?2, ?3, ?4)",
            vals(vec![
                team_id.into(),
                cfg.seed_team_name.clone().into(),
                cfg.seed_team_key.clone().into(),
                now.clone().into(),
            ]),
        )
        .await?;
    }

    conn.execute(
        "INSERT OR IGNORE INTO team_members (team_id, user_id) VALUES (?1, ?2)",
        vals(vec![team_id.into(), viewer_id.into()]),
    )
    .await?;

    ensure_workflow_state(conn, team_id, "Backlog", "unstarted", 0).await?;
    ensure_workflow_state(conn, team_id, "In Progress", "started", 1).await?;
    ensure_workflow_state(conn, team_id, "In Review", "started", 2).await?;
    ensure_workflow_state(conn, team_id, "Done", "completed", 3).await?;
    ensure_workflow_state(conn, team_id, "Canceled", "canceled", 4).await?;

    Ok(())
}

async fn ensure_workflow_state(
    conn: &Connection,
    team_id: &str,
    name: &str,
    kind: &str,
    position: i64,
) -> Result<()> {
    let c = count(
        conn,
        "SELECT COUNT(*) as value FROM workflow_states WHERE team_id = ?1 AND name = ?2",
        vec![team_id.to_string().into(), name.to_string().into()],
    )
    .await?;
    if c == 0 {
        let id = format!("state_{}", short_id());
        conn.execute(
            "INSERT INTO workflow_states (id, team_id, name, type, position) VALUES (?1, ?2, ?3, ?4, ?5)",
            vals(vec![
                id.into(),
                team_id.to_string().into(),
                name.to_string().into(),
                kind.to_string().into(),
                position.into(),
            ]),
        )
        .await?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct CountRow {
    value: i64,
}

async fn count(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<i64> {
    let rows: Vec<CountRow> = fetch_all(conn, sql, params).await?;
    Ok(rows.first().map(|r| r.value).unwrap_or(0))
}

async fn fetch_all<T>(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut rows = conn.query(sql, params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let parsed =
            de::from_row::<T>(&row).map_err(|e| anyhow::anyhow!("row decode failed: {e}"))?;
        out.push(parsed);
    }
    Ok(out)
}

async fn fetch_one<T>(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut rows = fetch_all(conn, sql, params).await?;
    Ok(rows.drain(..).next())
}

#[derive(Clone, Default)]
struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn viewer(&self, ctx: &Context<'_>) -> GqlResult<Viewer> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        get_viewer(&app.conn).await.map_err(gql_error)
    }

    async fn teams(
        &self,
        ctx: &Context<'_>,
        filter: Option<TeamsFilter>,
        first: Option<i32>,
    ) -> GqlResult<TeamConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        list_teams(&app.conn, filter, first)
            .await
            .map_err(gql_error)
    }

    async fn team(&self, ctx: &Context<'_>, id: String) -> GqlResult<Option<Team>> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        get_team(&app.conn, &id).await.map_err(gql_error)
    }

    async fn projects(
        &self,
        ctx: &Context<'_>,
        filter: Option<ProjectsFilter>,
        first: Option<i32>,
    ) -> GqlResult<ProjectConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        list_projects(&app.conn, filter, first)
            .await
            .map_err(gql_error)
    }

    async fn project(&self, ctx: &Context<'_>, id: String) -> GqlResult<Option<Project>> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        let project = get_project(&app.conn, &id).await.map_err(gql_error)?;
        if project.is_none() {
            return Err(Error::new("Entity not found: Project"));
        }
        Ok(project)
    }

    async fn issue(&self, ctx: &Context<'_>, id: String) -> GqlResult<Option<Issue>> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        let issue = get_issue(&app.conn, &id).await.map_err(gql_error)?;
        if issue.is_none() {
            return Err(Error::new("Entity not found: Issue"));
        }
        Ok(issue)
    }

    async fn issues(
        &self,
        ctx: &Context<'_>,
        filter: Option<IssuesFilter>,
        first: Option<i32>,
        order_by: Option<IssueOrderBy>,
    ) -> GqlResult<IssueConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        list_issues(&app.conn, filter, first, order_by)
            .await
            .map_err(gql_error)
    }

    async fn workflow_states(
        &self,
        ctx: &Context<'_>,
        filter: Option<WorkflowStatesFilter>,
    ) -> GqlResult<WorkflowStateConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        list_workflow_states(&app.conn, filter)
            .await
            .map_err(gql_error)
    }
}

#[derive(Clone, Default)]
struct MutationRoot;

#[Object]
impl MutationRoot {
    async fn project_create(
        &self,
        ctx: &Context<'_>,
        input: ProjectCreateInput,
    ) -> GqlResult<ProjectCreatePayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        create_project(&app.conn, &app.base_url, input)
            .await
            .map_err(gql_error)
    }

    async fn issue_create(
        &self,
        ctx: &Context<'_>,
        input: IssueCreateInput,
    ) -> GqlResult<IssueCreatePayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        create_issue(&app.conn, &app.base_url, input)
            .await
            .map_err(gql_error)
    }

    async fn comment_create(
        &self,
        ctx: &Context<'_>,
        input: CommentCreateInput,
    ) -> GqlResult<CommentCreatePayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        create_comment(&app.conn, &app.base_url, input)
            .await
            .map_err(gql_error)
    }

    async fn issue_update(
        &self,
        ctx: &Context<'_>,
        id: String,
        input: IssueUpdateInput,
    ) -> GqlResult<IssueUpdatePayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        update_issue(&app.conn, &id, input).await.map_err(gql_error)
    }

    async fn issue_archive(&self, ctx: &Context<'_>, id: String) -> GqlResult<IssueArchivePayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        archive_issue(&app.conn, &id).await.map_err(gql_error)
    }

    async fn issue_add_label(
        &self,
        ctx: &Context<'_>,
        id: String,
        label_id: String,
    ) -> GqlResult<IssueAddLabelPayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        add_label(&app.conn, &id, &label_id)
            .await
            .map_err(gql_error)
    }

    async fn admin_import_project(
        &self,
        ctx: &Context<'_>,
        input: AdminImportProjectInput,
    ) -> GqlResult<AdminImportProjectPayload> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        import_project_1to1(&app.conn, input)
            .await
            .map_err(gql_error)
    }
}

#[derive(Clone, SimpleObject)]
struct TeamConnection {
    nodes: Vec<Team>,
}

#[derive(Clone, SimpleObject)]
struct ProjectConnection {
    nodes: Vec<Project>,
}

#[derive(Clone, SimpleObject)]
struct IssueConnection {
    nodes: Vec<Issue>,
}

#[derive(Clone, SimpleObject)]
struct LabelConnection {
    nodes: Vec<Label>,
}

#[derive(Clone, SimpleObject)]
struct WorkflowStateConnection {
    nodes: Vec<WorkflowState>,
}

#[derive(Clone, SimpleObject)]
#[graphql(complex, rename_fields = "camelCase")]
struct Viewer {
    id: String,
    name: String,
    email: String,
}

#[ComplexObject]
impl Viewer {
    async fn teams(&self, ctx: &Context<'_>, first: Option<i32>) -> GqlResult<TeamConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        let limit = clamp_limit(first);
        let rows: Vec<TeamRow> = fetch_all(
            &app.conn,
            "SELECT t.id, t.name, t.key
             FROM teams t
             INNER JOIN team_members tm ON tm.team_id = t.id
             WHERE tm.user_id = ?1
             ORDER BY t.name ASC
             LIMIT ?2",
            vec![self.id.clone().into(), i64::from(limit).into()],
        )
        .await
        .map_err(gql_error)?;
        Ok(TeamConnection {
            nodes: rows.into_iter().map(Team::from).collect(),
        })
    }
}

#[derive(Clone, SimpleObject)]
#[graphql(complex, rename_fields = "camelCase")]
struct Team {
    id: String,
    name: String,
    key: String,
}

#[ComplexObject]
impl Team {
    async fn states(&self, ctx: &Context<'_>) -> GqlResult<WorkflowStateConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        let rows: Vec<WorkflowStateRow> = fetch_all(
            &app.conn,
            "SELECT id, name, type AS state_type
             FROM workflow_states
             WHERE team_id = ?1
             ORDER BY position ASC",
            vec![self.id.clone().into()],
        )
        .await
        .map_err(gql_error)?;
        Ok(WorkflowStateConnection {
            nodes: rows.into_iter().map(WorkflowState::from).collect(),
        })
    }
}

#[derive(Clone, SimpleObject)]
#[graphql(complex, rename_fields = "camelCase")]
struct Project {
    id: String,
    name: String,
    slug_id: Option<String>,
    state: Option<String>,
    archived_at: Option<String>,
    url: Option<String>,
}

#[ComplexObject]
impl Project {
    async fn issues(&self, ctx: &Context<'_>, first: Option<i32>) -> GqlResult<IssueConnection> {
        ensure_auth(ctx)?;
        let app = app_ctx(ctx);
        let limit = clamp_limit(first);
        let rows: Vec<IssueBaseRow> = fetch_all(
            &app.conn,
            &format!(
                "{} WHERE i.archived = 0 AND i.project_id = ?1 ORDER BY i.updated_at DESC LIMIT ?2",
                issue_base_select()
            ),
            vec![self.id.clone().into(), i64::from(limit).into()],
        )
        .await
        .map_err(gql_error)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in rows {
            issues.push(issue_from_row(&app.conn, row).await.map_err(gql_error)?);
        }
        Ok(IssueConnection { nodes: issues })
    }
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct Issue {
    id: String,
    identifier: String,
    title: String,
    url: String,
    description: Option<String>,
    assignee: Option<User>,
    project: Option<Project>,
    state: WorkflowState,
    labels: LabelConnection,
    updated_at: Option<String>,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct User {
    id: String,
    name: String,
    email: String,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct Label {
    id: String,
    name: String,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct WorkflowState {
    id: String,
    name: String,
    #[graphql(name = "type")]
    r#type: Option<String>,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct Comment {
    id: String,
    body: String,
    url: String,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct ProjectCreatePayload {
    success: bool,
    project: Project,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct IssueCreatePayload {
    success: bool,
    issue: Issue,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct CommentCreatePayload {
    success: bool,
    comment: Comment,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct IssueUpdatePayload {
    success: bool,
    issue: Issue,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct IssueArchivePayload {
    success: bool,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct IssueAddLabelPayload {
    success: bool,
}

#[derive(Clone, SimpleObject)]
#[graphql(rename_fields = "camelCase")]
struct AdminImportProjectPayload {
    success: bool,
    project: Project,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct StringFilter {
    eq: Option<String>,
    neq: Option<String>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct IdFilter {
    eq: Option<String>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct FloatFilter {
    #[graphql(name = "in")]
    in_values: Option<Vec<f64>>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct TeamFilter {
    id: Option<IdFilter>,
    key: Option<StringFilter>,
    name: Option<StringFilter>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct ProjectFilter {
    id: Option<IdFilter>,
    name: Option<StringFilter>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct StateFilter {
    name: Option<StringFilter>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct IssuesFilter {
    team: Option<TeamFilter>,
    project: Option<ProjectFilter>,
    state: Option<StateFilter>,
    number: Option<FloatFilter>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct TeamsFilter {
    name: Option<StringFilter>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct ProjectsFilter {
    name: Option<StringFilter>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct WorkflowStatesFilter {
    team: Option<TeamFilter>,
}

#[derive(InputObject, Clone)]
#[graphql(rename_fields = "camelCase")]
struct ProjectCreateInput {
    team_ids: Vec<String>,
    name: String,
}

#[derive(InputObject, Clone)]
#[graphql(rename_fields = "camelCase")]
struct IssueCreateInput {
    team_id: String,
    project_id: Option<String>,
    title: String,
    description: Option<String>,
}

#[derive(InputObject, Clone, Default)]
#[graphql(rename_fields = "camelCase")]
struct IssueUpdateInput {
    title: Option<String>,
    description: Option<String>,
    state_id: Option<String>,
}

#[derive(InputObject, Clone)]
#[graphql(rename_fields = "camelCase")]
struct CommentCreateInput {
    issue_id: String,
    body: String,
}

#[derive(InputObject, Clone)]
#[graphql(rename_fields = "camelCase")]
struct AdminImportProjectInput {
    id: String,
    name: String,
    slug_id: String,
    state: Option<String>,
    archived_at: Option<String>,
    url: String,
}

#[derive(Enum, Clone, Copy, Eq, PartialEq)]
enum IssueOrderBy {
    #[graphql(name = "updatedAt")]
    UpdatedAt,
}

#[derive(Deserialize)]
struct UserRow {
    id: String,
    name: String,
    email: String,
}

impl From<UserRow> for User {
    fn from(v: UserRow) -> Self {
        Self {
            id: v.id,
            name: v.name,
            email: v.email,
        }
    }
}

#[derive(Deserialize)]
struct TeamRow {
    id: String,
    name: String,
    key: String,
}

impl From<TeamRow> for Team {
    fn from(v: TeamRow) -> Self {
        Self {
            id: v.id,
            name: v.name,
            key: v.key,
        }
    }
}

#[derive(Deserialize)]
struct ProjectRow {
    id: String,
    name: String,
    slug_id: Option<String>,
    state: Option<String>,
    archived_at: Option<String>,
    url: Option<String>,
}

impl From<ProjectRow> for Project {
    fn from(v: ProjectRow) -> Self {
        Self {
            id: v.id,
            name: v.name,
            slug_id: v.slug_id,
            state: v.state,
            archived_at: v.archived_at,
            url: v.url,
        }
    }
}

#[derive(Deserialize)]
struct WorkflowStateRow {
    id: String,
    name: String,
    state_type: Option<String>,
}

impl From<WorkflowStateRow> for WorkflowState {
    fn from(v: WorkflowStateRow) -> Self {
        Self {
            id: v.id,
            name: v.name,
            r#type: v.state_type,
        }
    }
}

#[derive(Deserialize)]
struct LabelRow {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct IssueBaseRow {
    id: String,
    identifier: String,
    title: String,
    url: String,
    description: Option<String>,
    updated_at: Option<String>,
    ws_id: Option<String>,
    ws_name: Option<String>,
    ws_type: Option<String>,
    p_id: Option<String>,
    p_name: Option<String>,
    p_slug_id: Option<String>,
    p_state: Option<String>,
    p_archived_at: Option<String>,
    p_url: Option<String>,
    u_id: Option<String>,
    u_name: Option<String>,
    u_email: Option<String>,
}

async fn get_viewer(conn: &Connection) -> Result<Viewer> {
    let row: UserRow = fetch_one(
        conn,
        "SELECT id, name, email FROM users ORDER BY created_at ASC LIMIT 1",
        vec![],
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("no viewer configured"))?;
    Ok(Viewer {
        id: row.id,
        name: row.name,
        email: row.email,
    })
}

async fn list_teams(
    conn: &Connection,
    filter: Option<TeamsFilter>,
    first: Option<i32>,
) -> Result<TeamConnection> {
    let limit = clamp_limit(first);
    let mut where_sql = String::new();
    let mut params: Vec<Value> = Vec::new();
    if let Some(f) = filter {
        if let Some(name) = f.name.and_then(|v| v.eq).filter(|v| !v.is_empty()) {
            where_sql.push_str(" WHERE name = ?1");
            params.push(name.into());
        }
    }
    let limit_idx = params.len() + 1;
    let sql = format!(
        "SELECT id, name, key FROM teams{} ORDER BY name ASC LIMIT ?{}",
        where_sql, limit_idx
    );
    params.push(i64::from(limit).into());
    let rows: Vec<TeamRow> = fetch_all(conn, &sql, params).await?;
    Ok(TeamConnection {
        nodes: rows.into_iter().map(Team::from).collect(),
    })
}

async fn get_team(conn: &Connection, id: &str) -> Result<Option<Team>> {
    let row: Option<TeamRow> = fetch_one(
        conn,
        "SELECT id, name, key FROM teams WHERE id = ?1",
        vec![id.to_string().into()],
    )
    .await?;
    Ok(row.map(Team::from))
}

async fn list_projects(
    conn: &Connection,
    filter: Option<ProjectsFilter>,
    first: Option<i32>,
) -> Result<ProjectConnection> {
    let limit = clamp_limit(first);
    let mut where_sql = String::new();
    let mut params: Vec<Value> = Vec::new();
    if let Some(f) = filter {
        if let Some(name) = f.name.and_then(|v| v.eq).filter(|v| !v.is_empty()) {
            where_sql.push_str(" WHERE name = ?");
            params.push(name.into());
        }
    }
    let sql = format!(
        "SELECT id, name, slug_id, state, archived_at, url FROM projects{} ORDER BY created_at DESC LIMIT ?",
        where_sql
    );
    params.push(i64::from(limit).into());
    let rows: Vec<ProjectRow> = fetch_all(conn, &sql, params).await?;
    Ok(ProjectConnection {
        nodes: rows.into_iter().map(Project::from).collect(),
    })
}

async fn get_project(conn: &Connection, id: &str) -> Result<Option<Project>> {
    let row: Option<ProjectRow> = fetch_one(
        conn,
        "SELECT id, name, slug_id, state, archived_at, url FROM projects WHERE id = ?1",
        vec![id.to_string().into()],
    )
    .await?;
    Ok(row.map(Project::from))
}

async fn get_issue(conn: &Connection, id: &str) -> Result<Option<Issue>> {
    let sql = format!("{} WHERE i.id = ?1", issue_base_select());
    let row: Option<IssueBaseRow> = fetch_one(conn, &sql, vec![id.to_string().into()]).await?;
    match row {
        Some(v) => Ok(Some(issue_from_row(conn, v).await?)),
        None => Ok(None),
    }
}

async fn list_issues(
    conn: &Connection,
    filter: Option<IssuesFilter>,
    first: Option<i32>,
    _order_by: Option<IssueOrderBy>,
) -> Result<IssueConnection> {
    let limit = clamp_limit(first);
    let mut clauses = vec!["i.archived = 0".to_string()];
    let mut params: Vec<Value> = Vec::new();

    if let Some(filter) = filter {
        if let Some(team_id) = filter
            .team
            .as_ref()
            .and_then(|t| t.id.as_ref())
            .and_then(|v| v.eq.clone())
        {
            clauses.push("i.team_id = ?".to_string());
            params.push(team_id.into());
        }
        if let Some(team_key) = filter
            .team
            .as_ref()
            .and_then(|t| t.key.as_ref())
            .and_then(|k| k.eq.clone())
        {
            clauses.push("t.key = ?".to_string());
            params.push(team_key.into());
        }
        if let Some(project_id) = filter
            .project
            .as_ref()
            .and_then(|p| p.id.as_ref())
            .and_then(|v| v.eq.clone())
        {
            clauses.push("i.project_id = ?".to_string());
            params.push(project_id.into());
        }
        if let Some(state_name_eq) = filter
            .state
            .as_ref()
            .and_then(|s| s.name.as_ref())
            .and_then(|n| n.eq.clone())
        {
            clauses.push("ws.name = ?".to_string());
            params.push(state_name_eq.into());
        }
        if let Some(state_name_neq) = filter
            .state
            .as_ref()
            .and_then(|s| s.name.as_ref())
            .and_then(|n| n.neq.clone())
        {
            clauses.push("ws.name <> ?".to_string());
            params.push(state_name_neq.into());
        }
        if let Some(numbers) = filter
            .number
            .and_then(|n| n.in_values)
            .filter(|v| !v.is_empty())
        {
            let placeholders = std::iter::repeat_n("?", numbers.len())
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("i.number IN ({placeholders})"));
            for n in numbers {
                params.push((n as i64).into());
            }
        }
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "{}{} ORDER BY i.updated_at DESC LIMIT ?",
        issue_base_select(),
        where_sql
    );
    params.push(i64::from(limit).into());
    let rows: Vec<IssueBaseRow> = fetch_all(conn, &sql, params).await?;
    let mut issues = Vec::with_capacity(rows.len());
    for row in rows {
        issues.push(issue_from_row(conn, row).await?);
    }
    Ok(IssueConnection { nodes: issues })
}

async fn list_workflow_states(
    conn: &Connection,
    filter: Option<WorkflowStatesFilter>,
) -> Result<WorkflowStateConnection> {
    let mut params: Vec<Value> = Vec::new();
    let mut where_sql = String::new();
    if let Some(team_id) = filter
        .and_then(|f| f.team)
        .and_then(|t| t.id)
        .and_then(|id| id.eq)
    {
        where_sql = " WHERE team_id = ?1".to_string();
        params.push(team_id.into());
    }
    let sql = format!(
        "SELECT id, name, type AS state_type FROM workflow_states{} ORDER BY position ASC",
        where_sql
    );
    let rows: Vec<WorkflowStateRow> = fetch_all(conn, &sql, params).await?;
    Ok(WorkflowStateConnection {
        nodes: rows.into_iter().map(WorkflowState::from).collect(),
    })
}

async fn create_project(
    conn: &Connection,
    base_url: &str,
    input: ProjectCreateInput,
) -> Result<ProjectCreatePayload> {
    if input.team_ids.is_empty() {
        return Err(anyhow::anyhow!("teamIds must contain at least one team id"));
    }
    for team_id in &input.team_ids {
        let exists = count(
            conn,
            "SELECT COUNT(*) as value FROM teams WHERE id = ?1",
            vec![team_id.clone().into()],
        )
        .await?;
        if exists == 0 {
            return Err(anyhow::anyhow!("team not found: {team_id}"));
        }
    }

    let project_id = format!("project_{}", short_id());
    let slug = next_project_slug(conn, &input.name).await?;
    let now = now_iso();
    let url = format!("{}/project/{}", trim_trailing_slash(base_url), project_id);
    conn.execute(
        "INSERT INTO projects (id, name, slug_id, state, archived_at, url, created_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
        vals(vec![
            project_id.clone().into(),
            input.name.clone().into(),
            slug.clone().into(),
            "planned".into(),
            url.clone().into(),
            now.into(),
        ]),
    )
    .await?;

    for team_id in input.team_ids {
        conn.execute(
            "INSERT OR IGNORE INTO project_teams (project_id, team_id) VALUES (?1, ?2)",
            vals(vec![project_id.clone().into(), team_id.into()]),
        )
        .await?;
    }

    let project = Project {
        id: project_id,
        name: input.name,
        slug_id: Some(slug),
        state: Some("planned".to_string()),
        archived_at: None,
        url: Some(url),
    };

    Ok(ProjectCreatePayload {
        success: true,
        project,
    })
}

async fn create_issue(
    conn: &Connection,
    base_url: &str,
    input: IssueCreateInput,
) -> Result<IssueCreatePayload> {
    let team: TeamRow = fetch_one(
        conn,
        "SELECT id, name, key FROM teams WHERE id = ?1",
        vec![input.team_id.clone().into()],
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("team not found: {}", input.team_id))?;

    if let Some(ref project_id) = input.project_id {
        let exists = count(
            conn,
            "SELECT COUNT(*) as value FROM projects WHERE id = ?1",
            vec![project_id.clone().into()],
        )
        .await?;
        if exists == 0 {
            return Err(anyhow::anyhow!("project not found: {project_id}"));
        }
    }

    let state: WorkflowStateRow = fetch_one(
        conn,
        "SELECT id, name, type AS state_type
         FROM workflow_states
         WHERE team_id = ?1
         ORDER BY position ASC
         LIMIT 1",
        vec![team.id.clone().into()],
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("team {} has no workflow states", team.id))?;

    let next_number = count(
        conn,
        "SELECT COALESCE(MAX(number), 0) as value FROM issues WHERE team_id = ?1",
        vec![team.id.clone().into()],
    )
    .await?
        + 1;
    let identifier = format!("{}-{next_number}", team.key);
    let issue_id = format!("issue_{}", short_id());
    let url = format!("{}/issue/{}", trim_trailing_slash(base_url), identifier);
    let now = now_iso();
    conn.execute(
        "INSERT INTO issues
         (id, team_id, project_id, number, identifier, title, description, state_id, assignee_id, archived, url, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, 0, ?9, ?10, ?11)",
        vals(vec![
            issue_id.clone().into(),
            team.id.into(),
            option_string_to_value(input.project_id.clone()),
            next_number.into(),
            identifier.clone().into(),
            input.title.clone().into(),
            option_string_to_value(input.description.clone()),
            state.id.clone().into(),
            url.clone().into(),
            now.clone().into(),
            now.into(),
        ]),
    )
    .await?;

    let issue = get_issue(conn, &issue_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("failed to load created issue"))?;
    Ok(IssueCreatePayload {
        success: true,
        issue,
    })
}

async fn create_comment(
    conn: &Connection,
    base_url: &str,
    input: CommentCreateInput,
) -> Result<CommentCreatePayload> {
    let exists = count(
        conn,
        "SELECT COUNT(*) as value FROM issues WHERE id = ?1",
        vec![input.issue_id.clone().into()],
    )
    .await?;
    if exists == 0 {
        return Err(anyhow::anyhow!("issue not found: {}", input.issue_id));
    }
    let comment_id = format!("comment_{}", short_id());
    let url = format!("{}/comment/{}", trim_trailing_slash(base_url), comment_id);
    let now = now_iso();
    conn.execute(
        "INSERT INTO comments (id, issue_id, body, url, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        vals(vec![
            comment_id.clone().into(),
            input.issue_id.into(),
            input.body.clone().into(),
            url.clone().into(),
            now.into(),
        ]),
    )
    .await?;
    Ok(CommentCreatePayload {
        success: true,
        comment: Comment {
            id: comment_id,
            body: input.body,
            url,
        },
    })
}

async fn update_issue(
    conn: &Connection,
    issue_id: &str,
    input: IssueUpdateInput,
) -> Result<IssueUpdatePayload> {
    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(title) = input.title {
        sets.push("title = ?".to_string());
        params.push(title.into());
    }
    if let Some(description) = input.description {
        sets.push("description = ?".to_string());
        params.push(description.into());
    }
    if let Some(state_id) = input.state_id {
        sets.push("state_id = ?".to_string());
        params.push(state_id.into());
    }
    sets.push("updated_at = ?".to_string());
    params.push(now_iso().into());

    params.push(issue_id.to_string().into());
    let sql = format!("UPDATE issues SET {} WHERE id = ?", sets.join(", "));
    let changed = conn.execute(&sql, params).await?;
    if changed == 0 {
        return Err(anyhow::anyhow!("issue not found: {issue_id}"));
    }

    let issue = get_issue(conn, issue_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("failed to load updated issue"))?;
    Ok(IssueUpdatePayload {
        success: true,
        issue,
    })
}

async fn archive_issue(conn: &Connection, issue_id: &str) -> Result<IssueArchivePayload> {
    let changed = conn
        .execute(
            "UPDATE issues SET archived = 1, updated_at = ?1 WHERE id = ?2",
            vals(vec![now_iso().into(), issue_id.to_string().into()]),
        )
        .await?;
    Ok(IssueArchivePayload {
        success: changed > 0,
    })
}

async fn add_label(
    conn: &Connection,
    issue_id: &str,
    label_id: &str,
) -> Result<IssueAddLabelPayload> {
    let issue_exists = count(
        conn,
        "SELECT COUNT(*) as value FROM issues WHERE id = ?1",
        vec![issue_id.to_string().into()],
    )
    .await?
        > 0;
    if !issue_exists {
        return Ok(IssueAddLabelPayload { success: false });
    }

    conn.execute(
        "INSERT OR IGNORE INTO labels (id, name) VALUES (?1, ?2)",
        vals(vec![
            label_id.to_string().into(),
            label_id.to_string().into(),
        ]),
    )
    .await?;
    conn.execute(
        "INSERT OR IGNORE INTO issue_labels (issue_id, label_id) VALUES (?1, ?2)",
        vals(vec![
            issue_id.to_string().into(),
            label_id.to_string().into(),
        ]),
    )
    .await?;

    Ok(IssueAddLabelPayload { success: true })
}

async fn import_project_1to1(
    conn: &Connection,
    input: AdminImportProjectInput,
) -> Result<AdminImportProjectPayload> {
    conn.execute(
        "DELETE FROM projects WHERE slug_id = ?1 AND id <> ?2",
        vals(vec![input.slug_id.clone().into(), input.id.clone().into()]),
    )
    .await?;

    conn.execute(
        "INSERT INTO projects (id, name, slug_id, state, archived_at, url, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
           name = excluded.name,
           slug_id = excluded.slug_id,
           state = excluded.state,
           archived_at = excluded.archived_at,
           url = excluded.url",
        vals(vec![
            input.id.clone().into(),
            input.name.clone().into(),
            input.slug_id.clone().into(),
            option_string_to_value(input.state.clone()),
            option_string_to_value(input.archived_at.clone()),
            input.url.clone().into(),
            now_iso().into(),
        ]),
    )
    .await?;

    let project = get_project(conn, &input.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("failed to load imported project"))?;
    Ok(AdminImportProjectPayload {
        success: true,
        project,
    })
}

async fn issue_from_row(conn: &Connection, row: IssueBaseRow) -> Result<Issue> {
    let label_rows: Vec<LabelRow> = fetch_all(
        conn,
        "SELECT l.id, l.name
         FROM labels l
         INNER JOIN issue_labels il ON il.label_id = l.id
         WHERE il.issue_id = ?1
         ORDER BY l.name ASC",
        vec![row.id.clone().into()],
    )
    .await?;
    let labels = LabelConnection {
        nodes: label_rows
            .into_iter()
            .map(|l| Label {
                id: l.id,
                name: l.name,
            })
            .collect(),
    };

    let state = WorkflowState {
        id: row
            .ws_id
            .clone()
            .unwrap_or_else(|| "state_missing".to_string()),
        name: row.ws_name.clone().unwrap_or_else(|| "Backlog".to_string()),
        r#type: row.ws_type.clone(),
    };

    let project = row.p_id.map(|id| Project {
        id,
        name: row.p_name.unwrap_or_default(),
        slug_id: row.p_slug_id,
        state: row.p_state,
        archived_at: row.p_archived_at,
        url: row.p_url,
    });

    let assignee = row.u_id.map(|id| User {
        id,
        name: row.u_name.unwrap_or_default(),
        email: row.u_email.unwrap_or_default(),
    });

    Ok(Issue {
        id: row.id,
        identifier: row.identifier,
        title: row.title,
        url: row.url,
        description: row.description,
        assignee,
        project,
        state,
        labels,
        updated_at: row.updated_at,
    })
}

fn issue_base_select() -> &'static str {
    "SELECT
       i.id,
       i.identifier,
       i.title,
       i.url,
       i.description,
       i.updated_at,
       ws.id AS ws_id,
       ws.name AS ws_name,
       ws.type AS ws_type,
       p.id AS p_id,
       p.name AS p_name,
       p.slug_id AS p_slug_id,
       p.state AS p_state,
       p.archived_at AS p_archived_at,
       p.url AS p_url,
       u.id AS u_id,
       u.name AS u_name,
       u.email AS u_email
     FROM issues i
     LEFT JOIN workflow_states ws ON ws.id = i.state_id
     LEFT JOIN projects p ON p.id = i.project_id
     LEFT JOIN users u ON u.id = i.assignee_id
     LEFT JOIN teams t ON t.id = i.team_id"
}

async fn next_project_slug(conn: &Connection, project_name: &str) -> Result<String> {
    let base = slugify(project_name);
    let mut candidate = base.clone();
    let mut i = 2;
    loop {
        let exists = count(
            conn,
            "SELECT COUNT(*) as value FROM projects WHERE slug_id = ?1",
            vec![candidate.clone().into()],
        )
        .await?;
        if exists == 0 {
            return Ok(candidate);
        }
        candidate = format!("{base}-{i}");
        i += 1;
    }
}

fn option_string_to_value(v: Option<String>) -> Value {
    match v {
        Some(s) => Value::Text(s),
        None => Value::Null,
    }
}

fn vals(values: Vec<Value>) -> Vec<Value> {
    values
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

fn sanitize_team_key(input: &str) -> String {
    let key = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    if key.is_empty() {
        "SYN".to_string()
    } else {
        key
    }
}

fn clamp_limit(first: Option<i32>) -> i32 {
    first.unwrap_or(50).clamp(1, 500)
}

fn trim_trailing_slash(input: &str) -> &str {
    input.trim_end_matches('/')
}
