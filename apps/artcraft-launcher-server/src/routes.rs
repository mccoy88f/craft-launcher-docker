//! Who may ask for what, and the headers files go out with.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use artcraft_launcher_core::catalog::CraftApp;
use artcraft_launcher_core::hosted::{McpAnswer, McpChange, REQUEST_HEADER, ServerState};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::Server;
use crate::mcp::{self, Refusal};

type Shared = Arc<Server>;

pub fn router(server: Shared) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/state", get(state))
        .route("/api/releases", get(releases))
        .route("/api/apps/{app}/{action}", post(action))
        .route("/api/mcp", post(mcp_change))
        .route("/api/logout", post(crate::oauth::logout))
        .route("/admin/login", get(crate::oauth::login_page).post(crate::oauth::login))
        // Old addresses of the administrator's page: the sign-in, or the launcher once signed in.
        .route("/admin", get(admin_redirect))
        .route("/admin/", get(admin_redirect))
        .route("/admin/{*path}", get(admin_redirect))
        .route("/api/files/{app}/{client}/{*path}", get(file_get).put(file_put).layer(DefaultBodyLimit::max(mcp::FILE_MAX as usize)))
        .route("/files/{link}/{name}", get(link_get))
        .route("/mcp/{app}", post(mcp_post).get(mcp_get).delete(mcp_delete).layer(DefaultBodyLimit::max(32 << 20)))
        .route("/apps/{app}", get(app_redirect))
        .route("/apps/{app}/", get(app_file))
        .route("/apps/{app}/{*path}", get(app_file))
        .merge(crate::oauth::routes())
        .fallback(launcher_page)
        .with_state(server)
}

/// Whether the request comes from the signed-in administrator: the sign-in page's session
/// cookie, or the name and password over HTTP Basic (scripts).
pub fn is_admin(server: &Server, headers: &HeaderMap) -> bool {
    server.admin.as_ref().is_some_and(|a| server.sessions.signed_in(headers) || a.allows(headers))
}

/// What a request that needs the administrator gets otherwise. No `WWW-Authenticate`: the
/// launcher signs in on its own page, not through the browser's prompt.
async fn sign_in_first(headers: &HeaderMap) -> Response {
    if headers.contains_key(header::AUTHORIZATION) {
        // A wrong password costs a second, which makes guessing slow.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    no_store((StatusCode::UNAUTHORIZED, "Sign in as administrator first.").into_response())
}

async fn admin_redirect(State(server): State<Shared>, headers: HeaderMap) -> Response {
    Redirect::to(if is_admin(&server, &headers) { "/" } else { "/admin/login" }).into_response()
}

#[derive(Debug, Default, Deserialize)]
struct Options {
    pre: Option<String>,
    force: Option<String>,
}

fn on(flag: Option<&String>) -> bool {
    flag.is_some_and(|v| v == "1" || v == "true")
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// What everyone sees: the apps and the MCP server (tokens without their hints unless signed in).
fn view(server: &Server, admin: bool) -> ServerState {
    let mut state = server.hosting.state(admin, "apps/");
    state.mcp = Some(server.mcp.state(&server.hosting, "mcp/", admin));
    state
}

async fn state(State(server): State<Shared>, headers: HeaderMap) -> Response {
    no_store(Json(view(&server, is_admin(&server, &headers))).into_response())
}

async fn releases(State(server): State<Shared>, headers: HeaderMap, Query(o): Query<Options>) -> Response {
    // Only the administrator may make the server ask GitHub again at once.
    let force = on(o.force.as_ref()) && is_admin(&server, &headers);
    no_store(Json(server.releases.get(on(o.pre.as_ref()), force).await).into_response())
}

/// `POST /api/apps/<app>/<action>`: install, uninstall, pause or cancel (the administrator).
async fn action(State(server): State<Shared>, Path((app, action)): Path<(String, String)>, Query(o): Query<Options>, headers: HeaderMap) -> Response {
    // A form on another site can't set this header (see `hosted::REQUEST_HEADER`).
    if !headers.contains_key(REQUEST_HEADER) {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
    }
    if !is_admin(&server, &headers) {
        return sign_in_first(&headers).await;
    }
    let Some(app) = CraftApp::by_id(&app) else {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    };
    let result = match action.as_str() {
        "install" => match server.releases.latest(app.id, on(o.pre.as_ref())).await {
            Ok(release) => server.hosting.install(app, release, server.mcp.app_on(app.id)),
            Err(e) => Err(e),
        },
        // The app's MCP work folder goes with it (the uninstall prompt says so).
        "uninstall" => server.hosting.uninstall(app).map(|()| server.mcp.forget_app(app)),
        "pause" => {
            server.hosting.pause(app.id);
            Ok(())
        }
        "cancel" => {
            server.hosting.cancel(app.id);
            Ok(())
        }
        _ => return (StatusCode::NOT_FOUND, "no such action").into_response(),
    };
    match result {
        Ok(()) => no_store(Json(view(&server, true)).into_response()),
        Err(e) => no_store((StatusCode::CONFLICT, e).into_response()),
    }
}

/// `POST /api/mcp`: one [`McpChange`] (the administrator).
async fn mcp_change(State(server): State<Shared>, headers: HeaderMap, Json(change): Json<McpChange>) -> Response {
    if !headers.contains_key(REQUEST_HEADER) {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
    }
    if !is_admin(&server, &headers) {
        return sign_in_first(&headers).await;
    }
    let mcp = &server.mcp;
    let app = |id: &str| CraftApp::by_id(id).ok_or_else(|| "no such app".to_string());
    let mut token = None;
    let result: Result<(), String> = match change {
        McpChange::Enabled { on } => {
            mcp.set_enabled(on);
            Ok(())
        }
        McpChange::RetentionDays { days } => mcp.set_retention(days),
        McpChange::QuotaBytes { bytes } => mcp.set_quota(bytes),
        McpChange::MaxSessions { sessions } => mcp.set_max_sessions(sessions),
        McpChange::App { app: id, on } => match app(&id) {
            Ok(a) if on && server.hosting.site(a.id).is_none() => Err(format!("install {} first", a.name)),
            Ok(a) => match mcp.set_app(a, on) {
                // Its command-line tool comes from the release of the installed version.
                Ok(()) if on && server.hosting.cli(a.id).is_none() => match server.releases.latest(a.id, false).await {
                    Ok(release) => server.hosting.install_cli(a, release),
                    Err(e) => Err(e),
                },
                other => other,
            },
            Err(e) => Err(e),
        },
        McpChange::Empty { app: id } => app(&id).and_then(|a| mcp.empty(a)),
        McpChange::CreateToken { name, apps } => mcp.create_token(&name, apps).map(|t| token = Some(t)),
        McpChange::RevokeToken { id } => {
            mcp.revoke_token(&id);
            Ok(())
        }
        McpChange::FileFromUrl { on } => {
            mcp.set_file_from_url(on);
            Ok(())
        }
        McpChange::DeleteFile { app: id, client, path } => app(&id).and_then(|a| mcp.delete_file(a, &client, &path)),
    };
    match result {
        Ok(()) => no_store(Json(McpAnswer { state: view(&server, true), token }).into_response()),
        Err(e) => no_store((StatusCode::CONFLICT, e).into_response()),
    }
}

const SESSION_HEADER: &str = "mcp-session-id";

fn json_answer(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// Signs an MCP request in: the app, and the id of the token used.
fn mcp_sign_in(server: &Server, app: &str, headers: &HeaderMap) -> Result<(&'static CraftApp, String), Box<Response>> {
    // No check of `Origin`: every request needs its access token in the Authorization header,
    // which a page on another site (or a DNS-rebinding one) never has; clients such as claude.ai
    // send their own Origin.
    let Some(craft) = CraftApp::by_id(app) else {
        return Err(Box::new(json_answer(StatusCode::NOT_FOUND, mcp::rpc_error(None, -32600, "no such app"))));
    };
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let result = server.mcp.authorize(authorization, craft.id, &server.hosting);
    match &result {
        Err(Refusal::SignIn) if authorization.is_some() => log::info!("{}: MCP request with an unknown or expired token", craft.name),
        Err(Refusal::Forbidden) => log::warn!("{}: MCP request with a token that may not use it", craft.name),
        Err(Refusal::Off) => log::warn!("{}: MCP request while its MCP server is off (or its tool isn't installed)", craft.name),
        _ => {}
    }
    match result {
        Ok(token) => Ok((craft, token)),
        Err(Refusal::SignIn) => Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                // Where an OAuth client finds out how to sign in (MCP authorization, RFC 9728).
                [(
                    header::WWW_AUTHENTICATE,
                    format!(
                        "Bearer realm=\"ArtCraft Launcher MCP\", resource_metadata=\"{}\"",
                        crate::oauth::resource_metadata_url(headers, craft.id)
                    ),
                )],
                mcp::rpc_error(None, -32001, "a valid access token is needed (Authorization: Bearer …)"),
            )
                .into_response(),
        )),
        Err(Refusal::Forbidden) => Err(Box::new(json_answer(StatusCode::FORBIDDEN, mcp::rpc_error(None, -32001, "this token may not use this app")))),
        Err(Refusal::Off) => Err(Box::new(json_answer(StatusCode::NOT_FOUND, mcp::rpc_error(None, -32001, "this app's MCP server is off")))),
    }
}

/// `POST /mcp/<app>`: one JSON-RPC message from an MCP client.
async fn mcp_post(State(server): State<Shared>, Path(app): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let (craft, token) = match mcp_sign_in(&server, &app, &headers) {
        Ok(ok) => ok,
        Err(refused) => return *refused,
    };
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return json_answer(StatusCode::BAD_REQUEST, mcp::rpc_error(None, -32700, "not JSON"));
    };
    if !message.is_object() {
        return json_answer(StatusCode::BAD_REQUEST, mcp::rpc_error(None, -32600, "one JSON-RPC message per request"));
    }
    let id = message.get("id").filter(|_| message.get("method").is_some()).cloned();
    let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let session_id = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);

    let (session, new) = if method == "initialize" && session_id.is_none() {
        let Some(cli) = server.hosting.cli(craft.id) else {
            return json_answer(StatusCode::NOT_FOUND, mcp::rpc_error(id.as_ref(), -32001, "this app's MCP server is off"));
        };
        match server.mcp.start(craft, token.clone(), &cli) {
            Ok(s) => (s, true),
            Err(e) => {
                log::warn!("{}: couldn't start an MCP session ({e})", craft.name);
                return json_answer(StatusCode::SERVICE_UNAVAILABLE, mcp::rpc_error(id.as_ref(), -32000, &e));
            }
        }
    } else {
        let Some(sid) = session_id else {
            return json_answer(StatusCode::BAD_REQUEST, mcp::rpc_error(id.as_ref(), -32600, "missing Mcp-Session-Id: initialize first"));
        };
        match server.mcp.session(&sid, craft.id, &token) {
            Some(s) => (s, false),
            // MCP clients start a new session when they get 404.
            None => {
                log::info!("{}: MCP request for a session that has ended; the client starts a new one", craft.name);
                return json_answer(StatusCode::NOT_FOUND, mcp::rpc_error(id.as_ref(), -32001, "no such session: initialize again"));
            }
        }
    };

    // The launcher's own tools are answered here, without the app.
    if method == "tools/call"
        && let Some(name) = message.pointer("/params/name").and_then(|n| n.as_str()).filter(|n| n.starts_with("launcher_"))
    {
        let args = message.pointer("/params/arguments").cloned().unwrap_or(serde_json::Value::Null);
        let base = crate::oauth::public_base(&headers);
        let result = server.mcp.call_launcher_tool(&server.http, craft.id, &token, name, &args, &base).await;
        return json_answer(StatusCode::OK, serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
    }

    if method == "tools/call"
        && let Some((used, quota)) = server.mcp.over_quota(craft.id, &token)
    {
        let text =
            format!("the work folder is full ({} MB of {} MB): empty it, or raise the quota, in the launcher's settings", used >> 20, quota >> 20);
        return json_answer(StatusCode::OK, mcp::rpc_error(id.as_ref(), -32000, &text));
    }

    let rx = match session.send(&message).await {
        Ok(rx) => rx,
        Err(e) => {
            server.mcp.end_session(&session.id);
            return json_answer(StatusCode::BAD_GATEWAY, mcp::rpc_error(id.as_ref(), -32000, &e));
        }
    };
    let (Some(rx), Some(id)) = (rx, id) else {
        // A notification or a client's answer: nothing comes back.
        return StatusCode::ACCEPTED.into_response();
    };
    // The launcher adds to two answers: `initialize` (what the client should know about its
    // files) and the first page of `tools/list` (its own tools). Those come back as JSON.
    let amend = method == "initialize" || (method == "tools/list" && message.pointer("/params/cursor").is_none());
    let mut response = if amend {
        match mcp::answer(rx, &id).await {
            Some(line) => {
                let line = match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(mut answer) => {
                        if method == "initialize" {
                            let note = server.mcp.instructions(craft.id, &token);
                            if let Some(result) = answer.get_mut("result").and_then(|r| r.as_object_mut()) {
                                let text = match result.get("instructions").and_then(|i| i.as_str()) {
                                    Some(theirs) if !theirs.is_empty() => format!("{theirs}\n\n{note}"),
                                    _ => note,
                                };
                                result.insert("instructions".into(), text.into());
                            }
                        } else if let Some(tools) = answer.pointer_mut("/result/tools").and_then(|t| t.as_array_mut()) {
                            tools.extend(server.mcp.launcher_tools());
                        }
                        answer.to_string()
                    }
                    Err(_) => line,
                };
                json_answer(StatusCode::OK, line)
            }
            None => {
                log::warn!("{}: the app's MCP server didn't answer {method}", craft.name);
                if new {
                    server.mcp.end_session(&session.id);
                }
                json_answer(StatusCode::GATEWAY_TIMEOUT, mcp::rpc_error(Some(&id), -32000, "the app's MCP server didn't answer"))
            }
        }
    } else if headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/event-stream")) {
        // Everything the tool says until the answer, as it comes; the stream ends after the answer.
        use tokio_stream::StreamExt;
        let events = tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
            .map(|line| Ok::<_, std::convert::Infallible>(format!("event: message\ndata: {line}\n\n")));
        (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream"), (header::CACHE_CONTROL, "no-store")], Body::from_stream(events))
            .into_response()
    } else {
        match mcp::answer(rx, &id).await {
            Some(line) => json_answer(StatusCode::OK, line),
            None => {
                log::warn!("{}: the app's MCP server didn't answer {method}", craft.name);
                if new {
                    server.mcp.end_session(&session.id);
                }
                json_answer(StatusCode::GATEWAY_TIMEOUT, mcp::rpc_error(Some(&id), -32000, "the app's MCP server didn't answer"))
            }
        }
    };
    if new && let Ok(value) = HeaderValue::from_str(&session.id) {
        response.headers_mut().insert(SESSION_HEADER, value);
    }
    response
}

/// `GET /mcp/<app>`: no stream of messages the server starts on its own.
async fn mcp_get() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST, DELETE")]).into_response()
}

/// `DELETE /mcp/<app>`: the client ends its session.
async fn mcp_delete(State(server): State<Shared>, Path(app): Path<String>, headers: HeaderMap) -> Response {
    let (craft, token) = match mcp_sign_in(&server, &app, &headers) {
        Ok(ok) => ok,
        Err(refused) => return *refused,
    };
    let Some(sid) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match server.mcp.session(sid, craft.id, &token) {
        Some(s) => {
            server.mcp.end_session(&s.id);
            StatusCode::NO_CONTENT.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct FilePath {
    app: String,
    client: String,
    path: String,
}

/// `GET /api/files/<app>/<client>/<path>`: the administrator downloads a file of a client's work
/// folder.
async fn file_get(State(server): State<Shared>, Path(p): Path<FilePath>, headers: HeaderMap, request: Request) -> Response {
    if !is_admin(&server, &headers) {
        return sign_in_first(&headers).await;
    }
    match CraftApp::by_id(&p.app).and_then(|a| server.mcp.file_path(a.id, &p.client, &p.path)).filter(|f| f.is_file()) {
        Some(file) => send_file(file, request).await,
        None => (StatusCode::NOT_FOUND, "no such file").into_response(),
    }
}

/// `PUT /api/files/<app>/<client>/<path>`: the administrator puts a file in a client's work folder.
async fn file_put(State(server): State<Shared>, Path(p): Path<FilePath>, headers: HeaderMap, body: Bytes) -> Response {
    if !headers.contains_key(REQUEST_HEADER) {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
    }
    if !is_admin(&server, &headers) {
        return sign_in_first(&headers).await;
    }
    let Some(app) = CraftApp::by_id(&p.app) else {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    };
    match server.mcp.save_file(app, &p.client, &p.path, &body) {
        Ok(()) => no_store(Json(McpAnswer { state: view(&server, true), token: None }).into_response()),
        Err(e) => no_store((StatusCode::CONFLICT, e).into_response()),
    }
}

/// `GET /files/<link>/<name>`: a download link an MCP client asked for (`launcher_file_link`).
/// The link is the key: whoever has it may download the file, for an hour.
async fn link_get(State(server): State<Shared>, Path((link, _name)): Path<(String, String)>, request: Request) -> Response {
    match server.mcp.link(&link).filter(|f| f.is_file()) {
        Some(file) => send_file(file, request).await,
        None => (StatusCode::NOT_FOUND, "This download link has expired: ask for a new one.").into_response(),
    }
}

/// A file as a download, under its own name.
async fn send_file(file: PathBuf, request: Request) -> Response {
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let mut response = match ServeFile::new(&file).oneshot(request).await {
        Ok(r) => r.map(Body::new),
        Err(never) => match never {},
    };
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let disposition = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", artcraft_launcher_core::hosted::url_component(&name));
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    response
}

async fn app_redirect(Path(app): Path<String>) -> Response {
    match CraftApp::by_id(&app) {
        Some(app) => Redirect::permanent(&format!("{}/", app.id)).into_response(),
        None => (StatusCode::NOT_FOUND, "no such app").into_response(),
    }
}

/// A file of an installed app.
async fn app_file(State(server): State<Shared>, Path(params): Path<HashMap<String, String>>, request: Request) -> Response {
    let Some(app) = params.get("app").and_then(|a| CraftApp::by_id(a)) else {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    };
    let Some(site) = server.hosting.site(app.id) else {
        return (StatusCode::NOT_FOUND, format!("{} isn't installed on this server.", app.name)).into_response();
    };
    let prefix = format!("/apps/{}", app.id);
    // The app's page and its loader keep their names from version to version: everything is
    // revalidated (a 304 when unchanged), so an update reaches every visitor at once.
    serve(site, &prefix, request, false).await
}

/// The launcher itself, for visitors.
async fn launcher_page(State(server): State<Shared>, request: Request) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    serve(server.web_dir.clone(), "", request, true).await
}

/// Serves the file `request` names below `prefix` from `dir`. `hashed`: names carry a content
/// hash (trunk's `filehash`), so `.js` and `.wasm` can be cached for good.
async fn serve(dir: PathBuf, prefix: &str, request: Request, hashed: bool) -> Response {
    let (mut parts, body) = request.into_parts();
    let path = parts.uri.path().strip_prefix(prefix).unwrap_or("/");
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let Ok(uri) = path.parse::<Uri>() else {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    };
    parts.uri = uri;
    let request = Request::from_parts(parts, body);
    let mut response = match ServeDir::new(dir).append_index_html_on_directories(true).oneshot(request).await {
        Ok(r) => r.map(Body::new),
        Err(never) => match never {},
    };
    let immutable = hashed && (path.ends_with(".wasm") || path.ends_with(".js"));
    let headers = response.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth, hosting, releases};
    use base64::Engine;

    fn server(web: &std::path::Path, data: &std::path::Path, password: Option<&str>) -> Shared {
        let http = artcraft_launcher_native::http::Http::new().unwrap();
        Arc::new(Server {
            hosting: Arc::new(hosting::Hosting::open(data, http.clone()).unwrap()),
            mcp: Arc::new(crate::mcp::Mcp::open(data, data.join("work")).unwrap()),
            http: http.clone(),
            releases: releases::Releases::new(http, data, Duration::from_secs(1800)),
            web_dir: web.to_path_buf(),
            admin: password.map(|p| auth::Admin::new("admin".into(), p.into()).unwrap()),
            sessions: auth::Sessions::open(data),
        })
    }

    async fn send_body(router: &Router, method: &str, uri: &str, headers: &[(&str, String)], body: &str) -> Response {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap()
    }

    async fn send(router: &Router, method: &str, uri: &str, headers: &[(&str, String)]) -> Response {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    async fn text(response: Response) -> String {
        String::from_utf8(axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    fn sites() -> (tempfile::TempDir, tempfile::TempDir) {
        let web = tempfile::tempdir().unwrap();
        std::fs::write(web.path().join("index.html"), "launcher").unwrap();
        std::fs::write(web.path().join("artcraft-launcher-web-abc.wasm"), "\0asm").unwrap();
        std::fs::write(web.path().join("manifest.webmanifest"), "{}").unwrap();
        std::fs::create_dir_all(web.path().join("256x256/apps")).unwrap();
        std::fs::write(web.path().join("256x256/apps/icon.png"), "png").unwrap();
        let data = tempfile::tempdir().unwrap();
        let site = data.path().join("apps/photocraft/0.8.2-1");
        std::fs::create_dir_all(site.join("app")).unwrap();
        std::fs::write(site.join("index.html"), "photocraft").unwrap();
        std::fs::write(site.join("app/photocraft-web-abc.part-aa"), "parts").unwrap();
        let record = hosting::Record {
            version: "0.8.2".into(),
            tag: "v0.8.2".into(),
            dir: "0.8.2-1".into(),
            asset: "a.zip".into(),
            installed_at: 0,
            cli: None,
        };
        std::fs::write(data.path().join("apps/photocraft/installed.json"), serde_json::to_vec(&record).unwrap()).unwrap();
        (web, data)
    }

    #[tokio::test]
    async fn serves_the_launcher_and_installed_apps_to_everyone() {
        let (web, data) = sites();
        let router = router(server(web.path(), data.path(), Some("long enough")));
        let page = send(&router, "GET", "/", &[]).await;
        assert_eq!(page.headers()[header::CACHE_CONTROL], "no-cache");
        assert_eq!(text(page).await, "launcher");
        let wasm = send(&router, "GET", "/artcraft-launcher-web-abc.wasm", &[]).await;
        assert_eq!(wasm.headers()[header::CONTENT_TYPE], "application/wasm");
        assert!(wasm.headers()[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));

        assert_eq!(send(&router, "GET", "/apps/photocraft", &[]).await.headers()[header::LOCATION], "photocraft/");
        assert_eq!(text(send(&router, "GET", "/apps/photocraft/", &[]).await).await, "photocraft");
        let part = send(&router, "GET", "/apps/photocraft/app/photocraft-web-abc.part-aa", &[]).await;
        assert_eq!(part.headers()[header::CONTENT_TYPE], "application/octet-stream");
        assert!(part.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(send(&router, "GET", "/apps/photocraft/../../../etc/passwd", &[]).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(send(&router, "GET", "/apps/vectorcraft/", &[]).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(send(&router, "GET", "/apps/nope/", &[]).await.status(), StatusCode::NOT_FOUND);

        let state = text(send(&router, "GET", "/api/state", &[]).await).await;
        let state: artcraft_launcher_core::hosted::ServerState = serde_json::from_str(&state).unwrap();
        assert!(!state.admin);
        assert_eq!(state.apps_url, "apps/");
        assert!(state.apps.iter().any(|a| a.id == "photocraft" && a.installed.is_some()));
    }

    #[tokio::test]
    async fn only_the_administrator_changes_anything() {
        let (web, data) = sites();
        let router = router(server(web.path(), data.path(), Some("long enough")));
        let marker = (REQUEST_HEADER, "1".to_string());

        // Everyone sees the same page and state; nothing asks the browser for a password.
        let state = send(&router, "GET", "/api/state", &[]).await;
        assert!(state.headers().get(header::WWW_AUTHENTICATE).is_none());
        let state: ServerState = serde_json::from_str(&text(state).await).unwrap();
        assert!(!state.admin && state.mcp.is_some());
        assert_eq!(send(&router, "GET", "/admin/", &[]).await.headers()[header::LOCATION], "/admin/login");

        // Visitors can't remove anything.
        let refused = send(&router, "POST", "/api/apps/photocraft/uninstall", std::slice::from_ref(&marker)).await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        assert!(refused.headers().get(header::WWW_AUTHENTICATE).is_none());

        // The sign-in page: a wrong password stays there, the right one sets the session cookie.
        let form = ("content-type", "application/x-www-form-urlencoded".to_string());
        let page = text(send(&router, "GET", "/admin/login?next=/", &[]).await).await;
        assert!(page.contains("name=\"password\""));
        let wrong = send_body(&router, "POST", "/admin/login", std::slice::from_ref(&form), "user=admin&password=nope&next=/").await;
        assert_eq!(wrong.status(), StatusCode::OK);
        assert!(wrong.headers().get(header::SET_COOKIE).is_none());
        let ok = send_body(&router, "POST", "/admin/login", &[form], "user=admin&password=long%20enough&next=//evil.com").await;
        assert_eq!(ok.headers()[header::LOCATION], "/");
        let set = ok.headers()[header::SET_COOKIE].to_str().unwrap().to_string();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"));
        let cookie = ("cookie", set.split(';').next().unwrap().to_string());

        let state: ServerState = serde_json::from_str(&text(send(&router, "GET", "/api/state", std::slice::from_ref(&cookie)).await).await).unwrap();
        assert!(state.admin);
        assert_eq!(state.apps_url, "apps/");
        // A cross-site form can't send the marker header, even with the cookie.
        assert_eq!(send(&router, "POST", "/api/apps/photocraft/uninstall", std::slice::from_ref(&cookie)).await.status(), StatusCode::FORBIDDEN);
        assert!(data.path().join("apps/photocraft/0.8.2-1/index.html").is_file());

        let removed = send(&router, "POST", "/api/apps/photocraft/uninstall", &[cookie.clone(), marker.clone()]).await;
        assert_eq!(removed.status(), StatusCode::OK);
        for _ in 0..100 {
            if !data.path().join("apps/photocraft").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!data.path().join("apps/photocraft").exists());
        assert_eq!(send(&router, "GET", "/apps/photocraft/", &[]).await.status(), StatusCode::NOT_FOUND);

        // Signing out ends the session; scripts may still use HTTP Basic.
        assert_eq!(send(&router, "POST", "/api/logout", &[cookie.clone(), marker.clone()]).await.status(), StatusCode::NO_CONTENT);
        let state: ServerState = serde_json::from_str(&text(send(&router, "GET", "/api/state", &[cookie]).await).await).unwrap();
        assert!(!state.admin);
        let basic = ("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:long enough")));
        let state: ServerState = serde_json::from_str(&text(send(&router, "GET", "/api/state", &[basic]).await).await).unwrap();
        assert!(state.admin);
    }

    #[tokio::test]
    async fn without_a_password_there_is_no_administrator() {
        let (web, data) = sites();
        let router = router(server(web.path(), data.path(), None));
        let auth = ("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:")));
        let state: ServerState = serde_json::from_str(&text(send(&router, "GET", "/api/state", &[auth]).await).await).unwrap();
        assert!(!state.admin);
        assert_eq!(send(&router, "GET", "/apps/photocraft/", &[]).await.status(), StatusCode::OK);
    }
}
