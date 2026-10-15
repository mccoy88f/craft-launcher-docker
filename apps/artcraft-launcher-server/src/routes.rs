//! Who may ask for what, and the headers files go out with.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use artcraft_launcher_core::catalog::CraftApp;
use artcraft_launcher_core::hosted::REQUEST_HEADER;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeDir;

use crate::Server;

type Shared = Arc<Server>;

pub fn router(server: Shared) -> Router {
    let admin = Router::new()
        .route("/admin/api/state", get(admin_state))
        .route("/admin/api/releases", get(admin_releases))
        .route("/admin/api/apps/{app}/{action}", post(action))
        .route("/admin/", get(admin_page))
        .route("/admin/{*path}", get(admin_page))
        .route_layer(middleware::from_fn_with_state(server.clone(), require_admin));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/state", get(public_state))
        .route("/api/releases", get(public_releases))
        // Relative, so it still works when a proxy serves everything under a prefix.
        .route("/admin", get(|| async { Redirect::permanent("admin/") }))
        .route("/apps/{app}", get(app_redirect))
        .route("/apps/{app}/", get(app_file))
        .route("/apps/{app}/{*path}", get(app_file))
        .merge(admin)
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

async fn admin_state(State(server): State<Shared>) -> Response {
    no_store(Json(server.hosting.state(true, "../apps/")).into_response())
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
            Ok(release) => server.hosting.install(app, release),
            Err(e) => Err(e),
        },
        "uninstall" => server.hosting.uninstall(app),
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
        Ok(()) => no_store(Json(server.hosting.state(true, "../apps/")).into_response()),
        Err(e) => no_store((StatusCode::CONFLICT, e).into_response()),
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
        let data = tempfile::tempdir().unwrap();
        let site = data.path().join("apps/photocraft/0.8.2-1");
        std::fs::create_dir_all(site.join("app")).unwrap();
        std::fs::write(site.join("index.html"), "photocraft").unwrap();
        std::fs::write(site.join("app/photocraft-web-abc.part-aa"), "parts").unwrap();
        let record = hosting::Record { version: "0.8.2".into(), tag: "v0.8.2".into(), dir: "0.8.2-1".into(), asset: "a.zip".into(), installed_at: 0 };
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
