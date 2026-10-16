//! OAuth 2.1 for the MCP server, as MCP's authorization spec asks, so claude.ai, Claude's desktop
//! and mobile apps and other clients can connect by signing in instead of pasting a token.
//!
//! - `/.well-known/oauth-protected-resource[/mcp/<app>]` (RFC 9728) points clients here;
//!   a refused MCP request names it in its `WWW-Authenticate` header.
//! - `/.well-known/oauth-authorization-server` (RFC 8414) lists the endpoints.
//! - `/oauth/register` registers a client (RFC 7591); a client may instead use the https address
//!   of its metadata document as its `client_id`.
//! - `/oauth/authorize` shows a page where the administrator signs in (the launcher's own name and
//!   password) and allows or denies the client; codes need PKCE (S256).
//! - `/oauth/token` swaps a code for an access token (an hour) and a refresh token (rotated).
//!
//! The access tokens are the MCP server's own (`acl_…`), listed in Settings with the client's
//! name and revoked there like the rest.
//!
//! The same page style signs the administrator in to the launcher (`/admin/login`), with a
//! session cookie; signed in, the OAuth page asks for nothing but Allow or Deny.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use artcraft_launcher_core::catalog::CraftApp;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Json, Router, routing::get, routing::post};
use base64::Engine;
use serde::Deserialize;

use crate::Server;
use crate::mcp::ClientInfo;

type Shared = Arc<Server>;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource_metadata))
        .route("/.well-known/oauth-protected-resource/{*path}", get(resource_metadata_for))
        .route("/.well-known/oauth-authorization-server", get(server_metadata))
        .route("/.well-known/oauth-authorization-server/{*path}", get(server_metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_page).post(authorize_decision))
        .route("/oauth/token", post(token))
}

/// The server's public address (`https://launcher.example.com`): `ARTCRAFT_PUBLIC_URL`, or what
/// the reverse proxy says (`X-Forwarded-Proto`, `X-Forwarded-Host`), or the `Host` header.
pub fn public_base(headers: &HeaderMap) -> String {
    if let Ok(url) = std::env::var("ARTCRAFT_PUBLIC_URL")
        && !url.trim().is_empty()
    {
        return url.trim().trim_end_matches('/').to_string();
    }
    let first = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(str::trim);
    let proto = match first("x-forwarded-proto") {
        Some("https") => "https",
        _ => "http",
    };
    let host = first("x-forwarded-host").or_else(|| first("host")).unwrap_or("localhost");
    // Only what a host name and port are made of: nothing a header could smuggle into a URL.
    let host: String = host.chars().filter(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(*c)).take(255).collect();
    format!("{proto}://{host}")
}

/// Where the protected-resource metadata of `/mcp/<app>` is, for the `WWW-Authenticate` header.
pub fn resource_metadata_url(headers: &HeaderMap, app: &str) -> String {
    format!("{}/.well-known/oauth-protected-resource/mcp/{app}", public_base(headers))
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

async fn resource_metadata(headers: HeaderMap) -> Response {
    let base = public_base(&headers);
    Json(serde_json::json!({
        "resource": base,
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "resource_name": "ArtCraft Launcher",
    }))
    .into_response()
}

async fn resource_metadata_for(headers: HeaderMap, Path(path): Path<String>) -> Response {
    let base = public_base(&headers);
    let path: String = path.chars().filter(|c| c.is_ascii_alphanumeric() || "/-_".contains(*c)).collect();
    Json(serde_json::json!({
        "resource": format!("{base}/{}", path.trim_matches('/')),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "resource_name": "ArtCraft Launcher",
    }))
    .into_response()
}

async fn server_metadata(headers: HeaderMap) -> Response {
    let base = public_base(&headers);
    Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "client_id_metadata_document_supported": true,
        "scopes_supported": ["mcp"],
    }))
    .into_response()
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    no_store((status, Json(serde_json::json!({"error": error, "error_description": description}))).into_response())
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// `POST /oauth/register` (RFC 7591).
async fn register(State(server): State<Shared>, Json(r): Json<Registration>) -> Response {
    let method = r.token_endpoint_auth_method.unwrap_or_else(|| "none".into());
    let confidential = match method.as_str() {
        "none" => false,
        "client_secret_post" | "client_secret_basic" => true,
        _ => return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", "unsupported token_endpoint_auth_method"),
    };
    match server.mcp.register_client(r.client_name.as_deref().unwrap_or(""), r.redirect_uris, confidential) {
        Ok((client, secret)) => {
            log::info!("OAuth client registered: {}", client.client_name);
            let mut body = serde_json::json!({
                "client_id": client.client_id,
                "client_name": client.client_name,
                "redirect_uris": client.redirect_uris,
                "client_id_issued_at": client.created_at,
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": method,
            });
            if let Some(secret) = secret {
                body["client_secret"] = serde_json::Value::String(secret);
                body["client_secret_expires_at"] = serde_json::Value::from(0);
            }
            no_store((StatusCode::CREATED, Json(body)).into_response())
        }
        Err(e) => oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", &e),
    }
}

/// A client by id: registered here, or (an https `client_id`) read from its metadata document.
async fn find_client(server: &Server, client_id: &str) -> Result<ClientInfo, String> {
    if let Some(c) = server.mcp.client(client_id) {
        return Ok(c);
    }
    if !client_id.starts_with("https://") || client_id.len() > 512 {
        return Err("unknown client_id".into());
    }
    let http = server.http.clone();
    let url = client_id.to_string();
    let text = tokio::task::spawn_blocking(move || http.get_text(&url))
        .await
        .map_err(|_| "couldn't read the client's metadata".to_string())?
        .map_err(|e| format!("couldn't read the client's metadata ({e})"))?
        .ok_or("the client's metadata document wasn't found")?;
    let doc: serde_json::Value = serde_json::from_str(&text).map_err(|_| "the client's metadata document isn't JSON")?;
    if doc["client_id"].as_str() != Some(client_id) {
        return Err("the client's metadata document names another client_id".into());
    }
    let name = doc["client_name"].as_str().unwrap_or(client_id);
    let uris: Vec<String> =
        doc["redirect_uris"].as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect()).unwrap_or_default();
    Ok(server.mcp.remember_document(client_id, name, uris))
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Decision {
    #[serde(flatten)]
    params: AuthorizeParams,
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    decision: String,
}

/// The app a `resource` names (`https://host/mcp/photocraft`), if any.
fn resource_app(resource: Option<&str>) -> Option<&'static CraftApp> {
    let path = resource?.split("/mcp/").nth(1)?;
    CraftApp::by_id(path.trim_end_matches('/'))
}

/// Checks what can be checked before trusting `redirect_uri`; errors here are shown, not sent back.
async fn check_request(server: &Server, p: &AuthorizeParams) -> Result<ClientInfo, String> {
    if server.admin.is_none() {
        return Err("the administrator's page is switched off (no ARTCRAFT_ADMIN_PASSWORD)".into());
    }
    let client = find_client(server, &p.client_id).await?;
    if !client.redirect_uris.contains(&p.redirect_uri) {
        return Err("redirect_uri isn't one the client registered".into());
    }
    if p.response_type != "code" {
        return Err("response_type must be code".into());
    }
    if p.code_challenge_method != "S256" || p.code_challenge.len() < 43 || p.code_challenge.len() > 128 {
        return Err("a PKCE code_challenge (S256) is needed".into());
    }
    Ok(client)
}

/// `redirect_uri` with these query parameters added.
fn redirect_with(redirect_uri: &str, pairs: &[(&str, &str)]) -> String {
    let mut url = redirect_uri.to_string();
    for (i, (k, v)) in pairs.iter().enumerate() {
        let sep = if i == 0 && !redirect_uri.contains('?') { '?' } else { '&' };
        url.push(sep);
        url.push_str(k);
        url.push('=');
        url.push_str(&encode(v));
    }
    url
}

fn encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn authorize_page(State(server): State<Shared>, headers: HeaderMap, Query(p): Query<AuthorizeParams>) -> Response {
    let lang = Lang::from_headers(&headers);
    match check_request(&server, &p).await {
        Ok(client) => consent(&lang, &server, &client, &p, None, crate::routes::is_admin(&server, &headers)),
        Err(why) => error_page(&lang, &why),
    }
}

async fn authorize_decision(State(server): State<Shared>, headers: HeaderMap, Form(d): Form<Decision>) -> Response {
    let lang = Lang::from_headers(&headers);
    let client = match check_request(&server, &d.params).await {
        Ok(c) => c,
        Err(why) => return error_page(&lang, &why),
    };
    let p = &d.params;
    let back = |pairs: &[(&str, &str)]| {
        let mut all: Vec<(&str, &str)> = pairs.to_vec();
        if let Some(state) = p.state.as_deref() {
            all.push(("state", state));
        }
        Redirect::to(&redirect_with(&p.redirect_uri, &all)).into_response()
    };
    if d.decision != "allow" {
        return back(&[("error", "access_denied")]);
    }
    let allowed = crate::routes::is_admin(&server, &headers) || server.admin.as_ref().is_some_and(|a| a.allows_login(&d.user, &d.password));
    if !allowed {
        log::warn!("wrong administrator password on the OAuth page");
        tokio::time::sleep(Duration::from_secs(1)).await;
        return consent(&lang, &server, &client, p, Some(lang.t("Wrong name or password.")), false);
    }
    let apps = resource_app(p.resource.as_deref()).map(|a| vec![a.id.to_string()]);
    match server.mcp.new_code(&client, &p.redirect_uri, &p.code_challenge, apps) {
        Ok(code) => {
            log::info!("OAuth: {} allowed", client.name);
            let iss = public_base(&headers);
            back(&[("code", &code), ("iss", &iss)])
        }
        Err(_) => back(&[("error", "server_error")]),
    }
}

#[derive(Debug, Default, Deserialize)]
struct TokenRequest {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// The client's id and secret from HTTP Basic (`client_secret_basic`), if sent that way.
fn basic_client(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (id, secret) = text.split_once(':')?;
    Some((id.to_string(), secret.to_string()))
}

/// `POST /oauth/token`.
async fn token(State(server): State<Shared>, headers: HeaderMap, Form(t): Form<TokenRequest>) -> Response {
    let (client_id, secret) = match basic_client(&headers) {
        Some((id, secret)) => (id, Some(secret)),
        None => (t.client_id.clone(), t.client_secret.clone()),
    };
    let Ok(client) = find_client(&server, &client_id).await else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client");
    };
    if !server.mcp.client_authenticates(&client, secret.as_deref()) {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "wrong client secret");
    }
    let result = match t.grant_type.as_str() {
        "authorization_code" => server.mcp.redeem_code(&t.code, &client.client_id, &t.redirect_uri, &t.code_verifier),
        "refresh_token" => server.mcp.refresh(&t.refresh_token, &client.client_id),
        _ => return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "authorization_code or refresh_token"),
    };
    match result {
        Ok(tokens) => no_store(Json(tokens).into_response()),
        Err(error) => oauth_error(StatusCode::BAD_REQUEST, error, "the grant isn't valid (anymore)"),
    }
}

// ---- the page ---------------------------------------------------------------------------------

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

const STYLE: &str = "\
:root{--bg:#f2f1ee;--ink:#121316;--muted:#4a4a50;--line:rgba(18,19,22,.18);--card:#f7f6f3;--bad:#b3261e}\
@media (prefers-color-scheme:dark){:root{--bg:#121316;--ink:#f2f1ee;--muted:#a9a9ae;--line:rgba(242,241,238,.2);--card:#1a1b1f;--bad:#ff8a80}}\
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:var(--bg);color:var(--ink);font:15px/1.5 system-ui,-apple-system,'Segoe UI',sans-serif;padding:16px}\
main{width:100%;max-width:440px;background:var(--card);border:1px solid var(--line);padding:28px}\
h1{font-size:22px;line-height:1.25;margin:0 0 12px}p{margin:0 0 12px;color:var(--muted)}\
.label{font:600 11px/1.4 ui-monospace,'SF Mono',Menlo,monospace;letter-spacing:.14em;text-transform:uppercase;color:var(--muted);margin:16px 0 6px;display:block}\
input{width:100%;font:inherit;color:var(--ink);background:var(--bg);border:1px solid var(--line);padding:10px;border-radius:0}\
.apps{color:var(--ink)}.error{color:var(--bad);font-weight:600}\
.row{display:flex;gap:10px;margin-top:22px}button{flex:1;min-height:44px;font:600 12px ui-monospace,Menlo,monospace;\
letter-spacing:.14em;text-transform:uppercase;border:1px solid var(--ink);background:transparent;color:var(--ink);cursor:pointer;border-radius:0}\
button.primary{background:var(--ink);color:var(--bg)}";

fn page(lang: &Lang, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"{}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\"><title>ArtCraft Launcher</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        lang.code
    );
    let mut response = Html(html).into_response();
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    // The page can't be framed (clickjacking) and runs no script.
    h.insert(header::X_FRAME_OPTIONS, header::HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' https: http://localhost:* http://127.0.0.1:*; frame-ancestors 'none'",
        ),
    );
    response
}

fn error_page(lang: &Lang, why: &str) -> Response {
    let mut r = page(
        lang,
        &format!("<h1>ArtCraft Launcher</h1><p class=\"error\">{}</p>", escape(&lang.fmt("This request isn't valid: {why}", &[("why", why)]))),
    );
    *r.status_mut() = StatusCode::BAD_REQUEST;
    r
}

fn consent(lang: &Lang, server: &Server, client: &ClientInfo, p: &AuthorizeParams, error: Option<&str>, signed_in: bool) -> Response {
    let host = p.redirect_uri.split("://").nth(1).and_then(|r| r.split(['/', '?']).next()).unwrap_or("");
    let apps = match resource_app(p.resource.as_deref()) {
        Some(app) => app.name.to_string(),
        None => lang.t("All apps").to_string(),
    };
    let hidden: String = [
        ("response_type", Some(p.response_type.as_str())),
        ("client_id", Some(p.client_id.as_str())),
        ("redirect_uri", Some(p.redirect_uri.as_str())),
        ("state", p.state.as_deref()),
        ("code_challenge", Some(p.code_challenge.as_str())),
        ("code_challenge_method", Some(p.code_challenge_method.as_str())),
        ("resource", p.resource.as_deref()),
        ("scope", p.scope.as_deref()),
    ]
    .iter()
    .filter_map(|(k, v)| v.map(|v| format!("<input type=\"hidden\" name=\"{k}\" value=\"{}\">", escape(v))))
    .collect();
    let off = if server.mcp.enabled() {
        String::new()
    } else {
        format!("<p class=\"error\">{}</p>", escape(lang.t("The MCP server is off: turn it on in the launcher's settings first.")))
    };
    let error = error.map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", escape(e))).unwrap_or_default();
    // Signed in already (the launcher's session): only Allow or Deny.
    let credentials = if signed_in { String::new() } else { credential_fields(lang) };
    let body = format!(
        "<h1>{title}</h1><p>{asks}</p><p>{back}</p>{off}\
         <span class=\"label\">{apps_label}</span><div class=\"apps\">{apps}</div>\
         <form method=\"post\" action=\"authorize\">{hidden}{credentials}\
         {error}\
         <div class=\"row\"><button type=\"submit\" name=\"decision\" value=\"deny\">{deny}</button>\
         <button class=\"primary\" type=\"submit\" name=\"decision\" value=\"allow\">{allow}</button></div></form>",
        title = escape(&lang.fmt("Connect {client} to ArtCraft Launcher", &[("client", &client.name)])),
        asks = escape(&lang.fmt(
            "{client} asks to use the Craft Apps on this server over MCP: to open, edit and save files in their work folders.",
            &[("client", &client.name)]
        )),
        back = escape(&lang.fmt("After you allow it, you go back to {host}.", &[("host", host)])),
        apps_label = escape(lang.t("Apps it may use")),
        apps = escape(&apps),
        deny = escape(lang.t("Deny")),
        allow = escape(lang.t("Allow")),
    );
    page(lang, &body)
}

/// The administrator's name and password fields.
fn credential_fields(lang: &Lang) -> String {
    format!(
        "<label class=\"label\" for=\"user\">{admin}</label><input id=\"user\" name=\"user\" autocomplete=\"username\" value=\"admin\">\
         <label class=\"label\" for=\"password\">{password}</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" autofocus>",
        admin = escape(lang.t("Administrator")),
        password = escape(lang.t("Password")),
    )
}

// ---- the administrator's sign-in --------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct NextQuery {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    next: Option<String>,
}

/// Where to go after signing in: a path on this site, nothing else (no open redirect).
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains(['\\', '\r', '\n']) && n.len() < 512 => n.to_string(),
        _ => "/".to_string(),
    }
}

fn session_cookie(headers: &HeaderMap, value: &str, max_age: i64) -> String {
    let secure = if public_base(headers).starts_with("https://") { "; Secure" } else { "" };
    format!("{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}", crate::auth::COOKIE_NAME)
}

fn login_form(lang: &Lang, next: &str, error: Option<&str>, off: bool) -> Response {
    let error = error.map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", escape(e))).unwrap_or_default();
    let off = if off { "<p class=\"error\">ARTCRAFT_ADMIN_PASSWORD</p>" } else { "" };
    let body = format!(
        "<h1>{title}</h1>{off}<form method=\"post\" action=\"login\"><input type=\"hidden\" name=\"next\" value=\"{next}\">{fields}{error}\
         <div class=\"row\"><button class=\"primary\" type=\"submit\">{sign_in}</button></div></form>",
        title = escape(lang.t("Sign in as administrator")),
        next = escape(next),
        fields = credential_fields(lang),
        sign_in = escape(lang.t("Sign in")),
    );
    page(lang, &body)
}

/// `GET /admin/login`.
pub async fn login_page(State(server): State<Shared>, headers: HeaderMap, Query(q): Query<NextQuery>) -> Response {
    let next = safe_next(q.next.as_deref());
    if crate::routes::is_admin(&server, &headers) {
        return Redirect::to(&next).into_response();
    }
    login_form(&Lang::from_headers(&headers), &next, None, server.admin.is_none())
}

/// `POST /admin/login`: the name and password; a session cookie, and back to where it started.
pub async fn login(State(server): State<Shared>, headers: HeaderMap, Form(f): Form<LoginForm>) -> Response {
    let lang = Lang::from_headers(&headers);
    let next = safe_next(f.next.as_deref());
    let Some(admin) = &server.admin else { return login_form(&lang, &next, None, true) };
    if !admin.allows_login(&f.user, &f.password) {
        log::warn!("wrong administrator password on the sign-in page");
        tokio::time::sleep(Duration::from_secs(1)).await;
        return login_form(&lang, &next, Some(lang.t("Wrong name or password.")), false);
    }
    match server.sessions.start() {
        Ok(value) => {
            log::info!("administrator signed in");
            let mut r = Redirect::to(&next).into_response();
            if let Ok(v) = session_cookie(&headers, &value, crate::auth::SESSION_LIFE).parse() {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            no_store(r)
        }
        Err(e) => login_form(&lang, &next, Some(&e), false),
    }
}

/// `POST /api/logout`: ends the session.
pub async fn logout(State(server): State<Shared>, headers: HeaderMap) -> Response {
    if !headers.contains_key(artcraft_launcher_core::hosted::REQUEST_HEADER) {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
    }
    if let Some(value) = crate::auth::cookie(&headers) {
        server.sessions.end(&value);
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = session_cookie(&headers, "", 0).parse() {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    no_store(r)
}

// ---- the page's languages: the launcher's own catalogs ----------------------------------------

/// The catalogs of the launcher's interface (`crates/ui-egui/src/i18n`), whose rows the page's
/// strings are in, so every language the launcher speaks has them.
const CATALOGS: [(&str, &str); 11] = [
    ("it", include_str!("../../../crates/ui-egui/src/i18n/it.tsv")),
    ("de", include_str!("../../../crates/ui-egui/src/i18n/de.tsv")),
    ("fr", include_str!("../../../crates/ui-egui/src/i18n/fr.tsv")),
    ("es", include_str!("../../../crates/ui-egui/src/i18n/es.tsv")),
    ("pt-BR", include_str!("../../../crates/ui-egui/src/i18n/pt-BR.tsv")),
    ("ru", include_str!("../../../crates/ui-egui/src/i18n/ru.tsv")),
    ("cs", include_str!("../../../crates/ui-egui/src/i18n/cs.tsv")),
    ("pl", include_str!("../../../crates/ui-egui/src/i18n/pl.tsv")),
    ("hu", include_str!("../../../crates/ui-egui/src/i18n/hu.tsv")),
    ("ja", include_str!("../../../crates/ui-egui/src/i18n/ja.tsv")),
    ("zh-CN", include_str!("../../../crates/ui-egui/src/i18n/zh-CN.tsv")),
];

/// The page's strings (English is the key, as in the launcher).
#[cfg(test)]
const PAGE_STRINGS: [&str; 13] = [
    "Connect {client} to ArtCraft Launcher",
    "{client} asks to use the Craft Apps on this server over MCP: to open, edit and save files in their work folders.",
    "After you allow it, you go back to {host}.",
    "Apps it may use",
    "Administrator",
    "Password",
    "Allow",
    "Deny",
    "Wrong name or password.",
    "The MCP server is off: turn it on in the launcher's settings first.",
    "This request isn't valid: {why}",
    "Sign in as administrator",
    "Sign in",
];

struct Lang {
    code: &'static str,
    rows: HashMap<&'static str, &'static str>,
}

impl Lang {
    /// The first language of `Accept-Language` the launcher speaks, else English.
    fn from_headers(headers: &HeaderMap) -> Self {
        let accept = headers.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let code = accept.split(',').filter_map(|part| pick(part.split(';').next().unwrap_or("").trim())).next();
        let source = code.and_then(|c| CATALOGS.iter().find(|(k, _)| *k == c));
        match source {
            Some((code, text)) => Lang { code, rows: parse(text) },
            None => Lang { code: "en", rows: HashMap::new() },
        }
    }

    fn t<'a>(&'a self, key: &'a str) -> &'a str {
        self.rows.get(key).copied().unwrap_or(key)
    }

    fn fmt(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut out = self.rows.get(key).copied().unwrap_or(key).to_string();
        for (k, v) in args {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

/// The catalog for a language tag (`it-IT` → `it`, `pt` → `pt-BR`, `zh-Hans` → `zh-CN`).
fn pick(tag: &str) -> Option<&'static str> {
    let tag = tag.to_ascii_lowercase().replace('_', "-");
    let primary = tag.split('-').next().unwrap_or("");
    match primary {
        "pt" => Some("pt-BR"),
        "zh" if !tag.contains("hant") && !tag.contains("-tw") && !tag.contains("-hk") => Some("zh-CN"),
        _ => CATALOGS.iter().map(|(k, _)| *k).find(|k| k.eq_ignore_ascii_case(primary)),
    }
}

/// Plain rows (empty context) of a catalog: `\tsource\ttranslation`.
fn parse(text: &'static str) -> HashMap<&'static str, &'static str> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let (context, source, translation) = (parts.next()?, parts.next()?, parts.next()?);
            (context.is_empty() && !line.starts_with('#')).then_some((source, translation))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_the_page() {
        for (code, text) in CATALOGS {
            let rows = parse(text);
            for key in PAGE_STRINGS {
                let t = rows.get(key).unwrap_or_else(|| panic!("{code} is missing {key:?}"));
                for ph in ["{client}", "{host}", "{why}"] {
                    assert_eq!(key.contains(ph), t.contains(ph), "{code}: {key:?}");
                }
            }
        }
    }

    #[test]
    fn languages_come_from_the_browser() {
        let lang = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT_LANGUAGE, v.parse().unwrap());
            Lang::from_headers(&h).code
        };
        assert_eq!(lang("it-IT,it;q=0.9,en;q=0.8"), "it");
        assert_eq!(lang("xx,pt-PT;q=0.5"), "pt-BR");
        assert_eq!(lang("zh-TW,en"), "en");
        assert_eq!(lang("en-US"), "en");
    }

    #[test]
    fn sign_in_only_goes_back_to_this_site() {
        assert_eq!(safe_next(Some("/")), "/");
        assert_eq!(safe_next(Some("/?lang=it")), "/?lang=it");
        assert_eq!(safe_next(Some("//evil.com")), "/");
        assert_eq!(safe_next(Some("https://evil.com")), "/");
        assert_eq!(safe_next(Some("/\\evil.com")), "/");
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn addresses_are_built_safely() {
        assert_eq!(redirect_with("https://claude.ai/cb", &[("code", "a b"), ("state", "x&y")]), "https://claude.ai/cb?code=a%20b&state=x%26y");
        assert_eq!(redirect_with("https://x/cb?k=1", &[("code", "c")]), "https://x/cb?k=1&code=c");
        let mut h = HeaderMap::new();
        h.insert("host", "launcher.example.com".parse().unwrap());
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(public_base(&h), "https://launcher.example.com");
        h.insert("host", "evil.com/\"><script>".parse().unwrap());
        assert_eq!(public_base(&h), "https://evil.comscript");
        assert_eq!(resource_app(Some("https://h/mcp/photocraft")).map(|a| a.id), Some("photocraft"));
        assert!(resource_app(Some("https://h/")).is_none());
    }
}
