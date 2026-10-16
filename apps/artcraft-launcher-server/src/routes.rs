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
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeDir;

use crate::Server;
use crate::mcp::{self, Refusal};

type Shared = Arc<Server>;

pub fn router(server: Shared) -> Router {
    let admin = Router::new()
        .route("/admin/api/state", get(admin_state))
        .route("/admin/api/releases", get(admin_releases))
        .route("/admin/api/apps/{app}/{action}", post(action))
        .route("/admin/api/mcp", post(mcp_change))
        .route("/admin/", get(admin_page))
        .route("/admin/{*path}", get(admin_page))
        .route_layer(middleware::from_fn_with_state(server.clone(), require_admin));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/state", get(public_state))
        .route("/api/releases", get(public_releases))
        // Relative, so it still works when a proxy serves everything under a prefix.
        .route("/admin", get(|| async { Redirect::permanent("admin/") }))
        // The app manifest and its icons, for the administrator's page too: browsers fetch them
        // without the sign-in, so behind it the page couldn't be installed as an app (PWA).
        .route("/admin/manifest.webmanifest", get(admin_page))
        .route("/admin/256x256/{*path}", get(admin_page))
        .route("/admin/512x512/{*path}", get(admin_page))
        .route("/mcp/{app}", post(mcp_post).get(mcp_get).delete(mcp_delete).layer(DefaultBodyLimit::max(32 << 20)))
        .route("/apps/{app}", get(app_redirect))
        .route("/apps/{app}/", get(app_file))
        .route("/apps/{app}/{*path}", get(app_file))
        .merge(admin)
        .merge(crate::oauth::routes())
        .fallback(launcher_page)
        .with_state(server)
}

/// Turns the browser's password prompt on for everything under `/admin/`.
async fn require_admin(State(server): State<Shared>, request: Request, next: Next) -> Response {
    let Some(admin) = &server.admin else {
        return (StatusCode::NOT_FOUND, "The administrator's page is switched off: set ARTCRAFT_ADMIN_PASSWORD to turn it on.").into_response();
    };
    if admin.allows(request.headers()) {
        return next.run(request).await;
    }
    if request.headers().contains_key(header::AUTHORIZATION) {
        // A wrong password costs a second, which makes guessing slow.
        log::warn!("wrong administrator password for {}", request.uri().path());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"ArtCraft Launcher\", charset=\"UTF-8\""), (header::CACHE_CONTROL, "no-store")],
        "Sign in to install, update and remove Craft Apps.",
    )
        .into_response()
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

async fn public_state(State(server): State<Shared>) -> Response {
    no_store(Json(server.hosting.state(false, "apps/")).into_response())
}

/// The administrator's view: the apps, and the MCP server.
fn admin_view(server: &Server) -> ServerState {
    let mut state = server.hosting.state(true, "../apps/");
    state.mcp = Some(server.mcp.state(&server.hosting, "../mcp/"));
    state
}

async fn admin_state(State(server): State<Shared>) -> Response {
    no_store(Json(admin_view(&server)).into_response())
}

async fn public_releases(State(server): State<Shared>, Query(o): Query<Options>) -> Response {
    no_store(Json(server.releases.get(on(o.pre.as_ref()), false).await).into_response())
}

async fn admin_releases(State(server): State<Shared>, Query(o): Query<Options>) -> Response {
    no_store(Json(server.releases.get(on(o.pre.as_ref()), on(o.force.as_ref())).await).into_response())
}

/// `POST /admin/api/apps/<app>/<action>`: install, uninstall, pause or cancel.
async fn action(State(server): State<Shared>, Path((app, action)): Path<(String, String)>, Query(o): Query<Options>, headers: HeaderMap) -> Response {
    // A form on another site can't set this header (see `hosted::REQUEST_HEADER`).
    if !headers.contains_key(REQUEST_HEADER) {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
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
        Ok(()) => no_store(Json(admin_view(&server)).into_response()),
        Err(e) => no_store((StatusCode::CONFLICT, e).into_response()),
    }
}

/// `POST /admin/api/mcp`: one [`McpChange`].
async fn mcp_change(State(server): State<Shared>, headers: HeaderMap, Json(change): Json<McpChange>) -> Response {
    if !headers.contains_key(REQUEST_HEADER) {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
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
    };
    match result {
        Ok(()) => no_store(Json(McpAnswer { state: admin_view(&server), token }).into_response()),
        Err(e) => no_store((StatusCode::CONFLICT, e).into_response()),
    }
}

const SESSION_HEADER: &str = "mcp-session-id";

fn json_answer(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// Signs an MCP request in: the app, and the id of the token used.
fn mcp_sign_in(server: &Server, app: &str, headers: &HeaderMap) -> Result<(&'static CraftApp, String), Box<Response>> {
    // Browsers send Origin; a page on another site must not reach the MCP server (DNS rebinding).
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        if origin.split_once("://").map(|(_, h)| h) != Some(host) {
            return Err(Box::new(json_answer(StatusCode::FORBIDDEN, mcp::rpc_error(None, -32600, "requests from other sites aren't allowed"))));
        }
    }
    let Some(craft) = CraftApp::by_id(app) else {
        return Err(Box::new(json_answer(StatusCode::NOT_FOUND, mcp::rpc_error(None, -32600, "no such app"))));
    };
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    match server.mcp.authorize(authorization, craft.id, &server.hosting) {
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
        match server.mcp.start(craft, token, &cli) {
            Ok(s) => (s, true),
            Err(e) => return json_answer(StatusCode::SERVICE_UNAVAILABLE, mcp::rpc_error(id.as_ref(), -32000, &e)),
        }
    } else {
        let Some(sid) = session_id else {
            return json_answer(StatusCode::BAD_REQUEST, mcp::rpc_error(id.as_ref(), -32600, "missing Mcp-Session-Id: initialize first"));
        };
        match server.mcp.session(&sid, craft.id, &token) {
            Some(s) => (s, false),
            // MCP clients start a new session when they get 404.
            None => return json_answer(StatusCode::NOT_FOUND, mcp::rpc_error(id.as_ref(), -32001, "no such session: initialize again")),
        }
    };

    if method == "tools/call"
        && let Some((used, quota)) = server.mcp.over_quota(craft.id)
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
    let mut response = if headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/event-stream")) {
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

/// The launcher itself, for the administrator.
async fn admin_page(State(server): State<Shared>, request: Request) -> Response {
    serve(server.web_dir.clone(), "/admin", request, true).await
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
        })
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
        let auth = |up: &str| ("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(up)));
        let marker = (REQUEST_HEADER, "1".to_string());

        let anon = send(&router, "GET", "/admin/", &[]).await;
        assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
        assert!(anon.headers()[header::WWW_AUTHENTICATE].to_str().unwrap().starts_with("Basic"));
        assert_eq!(send(&router, "GET", "/admin/", &[auth("admin:wrong password")]).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(text(send(&router, "GET", "/admin/", &[auth("admin:long enough")]).await).await, "launcher");
        assert_eq!(send(&router, "GET", "/admin", &[]).await.headers()[header::LOCATION], "admin/");
        // Browsers fetch the app manifest and its icons without the sign-in.
        assert_eq!(text(send(&router, "GET", "/admin/manifest.webmanifest", &[]).await).await, "{}");
        assert_eq!(send(&router, "GET", "/admin/256x256/apps/icon.png", &[]).await.status(), StatusCode::OK);
        assert_eq!(send(&router, "GET", "/admin/api/state", &[]).await.status(), StatusCode::UNAUTHORIZED);

        let state = text(send(&router, "GET", "/admin/api/state", &[auth("admin:long enough")]).await).await;
        let state: artcraft_launcher_core::hosted::ServerState = serde_json::from_str(&state).unwrap();
        assert!(state.admin);
        assert_eq!(state.apps_url, "../apps/");

        // Visitors can't remove anything, nor can a cross-site form without the marker header.
        assert_eq!(
            send(&router, "POST", "/admin/api/apps/photocraft/uninstall", std::slice::from_ref(&marker)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&router, "POST", "/api/apps/photocraft/uninstall", std::slice::from_ref(&marker)).await.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(send(&router, "POST", "/admin/api/apps/photocraft/uninstall", &[auth("admin:long enough")]).await.status(), StatusCode::FORBIDDEN);
        assert!(data.path().join("apps/photocraft/0.8.2-1/index.html").is_file());

        let removed = send(&router, "POST", "/admin/api/apps/photocraft/uninstall", &[auth("admin:long enough"), marker]).await;
        assert_eq!(removed.status(), StatusCode::OK);
        for _ in 0..100 {
            if !data.path().join("apps/photocraft").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!data.path().join("apps/photocraft").exists());
        assert_eq!(send(&router, "GET", "/apps/photocraft/", &[]).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn without_a_password_there_is_no_administrator() {
        let (web, data) = sites();
        let router = router(server(web.path(), data.path(), None));
        let auth = ("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:")));
        assert_eq!(send(&router, "GET", "/admin/", &[auth]).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(send(&router, "GET", "/apps/photocraft/", &[]).await.status(), StatusCode::OK);
    }
}
