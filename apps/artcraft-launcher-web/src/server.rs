//! The backend for a page served by a launcher server (`artcraft-launcher-server`): the apps are
//! the browser builds that server hosts, and Open opens one in a window of its own. Everyone sees
//! the same page; Install, Update, Uninstall and the MCP settings ask the server to do it when
//! signed in as its administrator, and go to the sign-in page (`admin/login`) otherwise.
//!
//! Every URL here is relative to the page, so the same build works at `/`, under `/admin/` and
//! behind a proxy's prefix. The API is described in `artcraft_launcher_core::hosted`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use artcraft_launcher_core::hosted::{McpAnswer, McpChange, McpState, REQUEST_HEADER, Releases, ServerState};
use artcraft_launcher_core::protocol::{Backend, Capabilities, Command, Event, LatestRelease, Origin, Source};
use artcraft_launcher_core::{Platform, SUITE, version};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::web::{PicturesBusy, fetch_screenshots, visitor_platform};

/// How often to look at the server while one of its jobs runs, and otherwise (the administrator
/// may install something while a visitor has the page open).
const BUSY_POLL: Duration = Duration::from_millis(700);
const IDLE_POLL: Duration = Duration::from_secs(30);

/// The server's state, when the page is served by a launcher server; `None` for a static site.
pub async fn probe() -> Option<ServerState> {
    let (status, text) = request("GET", "api/state").await.ok()?;
    if status != 200 {
        return None;
    }
    serde_json::from_str(&text).ok()
}

/// What polling has seen so far.
#[derive(Default)]
struct Seen {
    state: ServerState,
    /// The last finished job reported per app, so each ending is told once.
    finished: HashMap<String, u64>,
    in_flight: bool,
    /// When the state was last asked for (milliseconds since the epoch).
    asked_at: f64,
}

pub struct ServerBackend {
    events: Rc<RefCell<Vec<Event>>>,
    ctx: egui::Context,
    platform: Option<Platform>,
    admin: bool,
    prereleases: bool,
    checking: Rc<RefCell<bool>>,
    seen: Rc<RefCell<Seen>>,
    pictures_busy: PicturesBusy,
}

impl ServerBackend {
    pub fn new(ctx: egui::Context, state: ServerState) -> Self {
        let this = Self {
            events: Rc::default(),
            ctx,
            platform: visitor_platform(),
            admin: state.admin,
            prereleases: false,
            checking: Rc::default(),
            seen: Rc::default(),
            pictures_busy: Rc::default(),
        };
        {
            let mut seen = this.seen.borrow_mut();
            // Jobs that ended before the page opened aren't news.
            seen.finished = state.apps.iter().filter_map(|a| a.finished.as_ref().map(|f| (a.id.clone(), f.seq))).collect();
            seen.asked_at = js_sys::Date::now();
            apply(&mut seen, state, &mut this.events.borrow_mut(), true);
        }
        this
    }

    fn busy(&self) -> bool {
        self.seen.borrow().state.apps.iter().any(|a| a.job.is_some())
    }

    /// Asks the server what it's doing, unless that was asked a moment ago.
    fn refresh(&self, now: bool) {
        let interval = if self.busy() { BUSY_POLL } else { IDLE_POLL };
        {
            let mut seen = self.seen.borrow_mut();
            let due = js_sys::Date::now() - seen.asked_at >= interval.as_millis() as f64;
            if seen.in_flight || !(now || due) {
                return;
            }
            seen.in_flight = true;
            seen.asked_at = js_sys::Date::now();
        }
        let (seen, events, ctx) = (self.seen.clone(), self.events.clone(), self.ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let state = match request("GET", "api/state").await {
                Ok((200, text)) => serde_json::from_str::<ServerState>(&text).ok(),
                _ => None,
            };
            let mut seen = seen.borrow_mut();
            seen.in_flight = false;
            if let Some(state) = state {
                apply(&mut seen, state, &mut events.borrow_mut(), false);
                ctx.request_repaint();
            }
        });
    }

    /// Asks the server to install, uninstall, pause or cancel `app` (the administrator's page only).
    fn act(&self, app: String, action: &'static str) {
        if !self.admin {
            sign_in();
            return;
        }
        let pre = if self.prereleases && action == "install" { "?pre=1" } else { "" };
        let url = format!("api/apps/{app}/{action}{pre}");
        let (seen, events, ctx) = (self.seen.clone(), self.events.clone(), self.ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let answer = request("POST", &url).await;
            let mut seen = seen.borrow_mut();
            let mut events = events.borrow_mut();
            match answer {
                Ok((200, text)) => {
                    if let Ok(state) = serde_json::from_str::<ServerState>(&text) {
                        seen.asked_at = js_sys::Date::now();
                        apply(&mut seen, state, &mut events, false);
                    }
                }
                // The session ended meanwhile: sign in again.
                Ok((401, _)) => sign_in(),
                Ok((status, text)) => {
                    let why = if text.trim().is_empty() { format!("the launcher server answered HTTP {status}") } else { text.trim().to_string() };
                    events.push(Event::JobFinished { app, result: Err(why) });
                }
                Err(why) => events.push(Event::JobFinished { app, result: Err(why) }),
            }
            ctx.request_repaint();
        });
    }

    /// Asks the server to change its MCP server (the administrator's page only).
    fn mcp(&self, change: McpChange) {
        if !self.admin {
            sign_in();
            return;
        }
        let Ok(body) = serde_json::to_string(&change) else { return };
        let label = match &change {
            McpChange::App { app, .. } | McpChange::Empty { app } => app.clone(),
            _ => "mcp".to_string(),
        };
        let (seen, events, ctx) = (self.seen.clone(), self.events.clone(), self.ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let answer = request_with("POST", "api/mcp", Some(&body)).await;
            let mut seen = seen.borrow_mut();
            let mut events = events.borrow_mut();
            match answer {
                Ok((200, text)) => {
                    if let Ok(answer) = serde_json::from_str::<McpAnswer>(&text) {
                        seen.asked_at = js_sys::Date::now();
                        apply(&mut seen, answer.state, &mut events, false);
                        if let Some(token) = answer.token {
                            events.push(Event::McpTokenCreated(token));
                        }
                    }
                }
                Ok((401, _)) => sign_in(),
                Ok((status, text)) => {
                    let why = if text.trim().is_empty() { format!("the launcher server answered HTTP {status}") } else { text.trim().to_string() };
                    events.push(Event::JobFinished { app: label, result: Err(why) });
                }
                Err(why) => events.push(Event::JobFinished { app: label, result: Err(why) }),
            }
            ctx.request_repaint();
        });
    }

    fn check(&self, force: bool) {
        if self.checking.replace(true) {
            return;
        }
        let mut url = String::from("api/releases?");
        if self.prereleases {
            url.push_str("pre=1&");
        }
        if force && self.admin {
            url.push_str("force=1");
        }
        let (events, ctx, checking, admin) = (self.events.clone(), self.ctx.clone(), self.checking.clone(), self.admin);
        wasm_bindgen_futures::spawn_local(async move {
            events.borrow_mut().push(Event::CheckStarted);
            ctx.request_repaint();
            let answer = match request("GET", &url).await {
                Ok((200, text)) => serde_json::from_str::<Releases>(&text).map_err(|e| format!("unreadable answer from the launcher server ({e})")),
                Ok((status, _)) => Err(format!("the launcher server answered HTTP {status}")),
                Err(why) => Err(why),
            };
            let mut events = events.borrow_mut();
            match answer {
                Ok(r) => {
                    for app in r.apps {
                        events.push(Event::Latest { app: app.id, latest: app.latest, from: Origin::GitHub });
                    }
                    // Only the administrator can do anything about a newer launcher (a new image).
                    if admin
                        && let Some(l) = r.launcher
                        && let (Some(latest), Some(this)) = (l.version.as_ref(), version::parse(env!("CARGO_PKG_VERSION")))
                        && version::is_newer(latest, &this)
                    {
                        events.push(Event::LauncherUpdate(l));
                    }
                    events.push(Event::CheckFinished { at: r.checked_at, source: r.source, rate_limited_until: r.rate_limited_until });
                }
                Err(why) => {
                    for app in &SUITE {
                        events.push(Event::Latest { app: app.id.to_string(), latest: LatestRelease::Failed(why.clone()), from: Origin::GitHub });
                    }
                    let at = (js_sys::Date::now() / 1000.0) as i64;
                    events.push(Event::CheckFinished { at, source: Source::GitHubApi, rate_limited_until: None });
                }
            }
            *checking.borrow_mut() = false;
            ctx.request_repaint();
        });
    }
}

/// Turns what changed between the last state and `state` into events.
fn apply(seen: &mut Seen, state: ServerState, events: &mut Vec<Event>, first: bool) {
    for app in &state.apps {
        let before = seen.state.apps.iter().find(|a| a.id == app.id);
        if first || before.is_none_or(|b| b.installed != app.installed) {
            events.push(Event::Detected { app: app.id.clone(), installed: app.installed.clone() });
        }
        if first || before.is_none_or(|b| b.paused != app.paused) {
            events.push(Event::Paused { app: app.id.clone(), download: app.paused });
        }
        if let Some(f) = &app.finished
            && seen.finished.get(&app.id).is_none_or(|last| f.seq > *last)
        {
            seen.finished.insert(app.id.clone(), f.seq);
            events.push(Event::JobFinished { app: app.id.clone(), result: f.result.clone() });
        }
        if let Some(phase) = app.job
            && (first || before.is_none_or(|b| b.job != Some(phase)))
        {
            events.push(Event::Progress { app: app.id.clone(), phase });
        }
    }
    if let Some(mcp) = &state.mcp
        && (first || seen.state.mcp.as_ref() != Some(mcp))
    {
        events.push(Event::Mcp(McpState { base_url: absolute(&mcp.base_url), ..mcp.clone() }));
    }
    seen.state = state;
}

/// `url` resolved against the page's address (`../mcp/` → `https://example.com/mcp/`).
fn absolute(url: &str) -> String {
    web_sys::window()
        .and_then(|w| w.location().href().ok())
        .and_then(|base| web_sys::Url::new_with_base(url, &base).ok())
        .map_or_else(|| url.to_string(), |u| u.href())
}

impl Backend for ServerBackend {
    fn send(&mut self, command: Command) {
        match command {
            Command::CheckForUpdates { force } => self.check(force),
            Command::Detect => self.refresh(true),
            Command::Install { app } => self.act(app, "install"),
            Command::Uninstall { app } => self.act(app, "uninstall"),
            Command::Pause { app } => self.act(app, "pause"),
            Command::Cancel { app } => self.act(app, "cancel"),
            Command::Launch { app } => {
                let installed = self.seen.borrow().state.apps.iter().any(|a| a.id == app && a.installed.is_some());
                if installed {
                    let url = self.seen.borrow().state.app_url(&app);
                    if !open_app_window(&app, &url) {
                        self.ctx.open_url(egui::OpenUrl::new_tab(url));
                    }
                }
            }
            Command::FetchScreenshots { app } => fetch_screenshots(&app, &self.events, &self.ctx, &self.pictures_busy),
            Command::Configure(o) => {
                let changed = o.prereleases != self.prereleases;
                self.prereleases = o.prereleases;
                if changed {
                    self.check(false);
                }
            }
            Command::Mcp(change) => self.mcp(change),
            Command::SignIn => sign_in(),
            Command::SignOut => sign_out(),
            Command::Reveal { .. } | Command::UpdateLauncher => {}
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        self.refresh(false);
        // Keep frames coming often enough to look at the server again.
        self.ctx.request_repaint_after(if self.busy() { BUSY_POLL } else { IDLE_POLL });
        std::mem::take(&mut *self.events.borrow_mut())
    }

    fn capabilities(&self) -> Capabilities {
        // Everything shows for everyone; what needs the administrator signs in first.
        Capabilities { manage_apps: true, reveal_label: "", self_update: false, install_apps: true, web_apps: true }
    }

    fn signed_in(&self) -> Option<bool> {
        Some(self.admin)
    }

    fn platform(&self) -> Option<Platform> {
        self.platform
    }

    fn now(&self) -> i64 {
        (js_sys::Date::now() / 1000.0) as i64
    }
}

/// Goes to the sign-in page, which comes back here once signed in.
fn sign_in() {
    let Some(window) = web_sys::window() else { return };
    let location = window.location();
    let here = format!("{}{}", location.pathname().unwrap_or_else(|_| "/".into()), location.search().unwrap_or_default());
    let next: String = js_sys::encode_uri_component(&here).into();
    let lang = artcraft_launcher_ui_egui::i18n::current().code();
    let _ = location.set_href(&format!("admin/login?next={next}&lang={lang}"));
}

/// Ends the session and shows the page again, as a visitor.
fn sign_out() {
    wasm_bindgen_futures::spawn_local(async {
        let _ = request("POST", "api/logout").await;
        if let Some(w) = web_sys::window() {
            let _ = w.location().reload();
        }
    });
}

/// Opens an app in a window of its own, as big as the screen allows: a pop-up window has no tabs
/// and no toolbar, so the app looks like an app (browsers keep a slim strip with its address).
/// When the launcher is installed as an app (PWA), the apps, which are in its scope, open in app
/// windows with no browser bars at all. Each app has its own window name, so opening it again
/// brings that window back. `false` when the browser refused (a pop-up blocker); the caller then
/// opens a tab instead.
fn open_app_window(app: &str, url: &str) -> bool {
    let Some(window) = web_sys::window() else { return false };
    let (width, height) =
        window.screen().ok().map_or((1280, 800), |s| (s.avail_width().unwrap_or(1280).max(640), s.avail_height().unwrap_or(800).max(480)));
    let features = format!("popup=yes,width={width},height={height},left=0,top=0");
    matches!(window.open_with_url_and_target_and_features(url, &format!("artcraft-{app}"), &features), Ok(Some(_)))
}

/// One request to the launcher server: its status and body. The browser sends the
/// administrator's sign-in along by itself (same origin, under `admin/`).
async fn request(method: &str, url: &str) -> Result<(u16, String), String> {
    request_with(method, url, None).await
}

/// [`request`] with a JSON body.
async fn request_with(method: &str, url: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let offline = || "couldn't reach the launcher server".to_string();
    let window = web_sys::window().ok_or_else(offline)?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_mode(web_sys::RequestMode::SameOrigin);
    if let Some(body) = body {
        init.set_body(&wasm_bindgen::JsValue::from_str(body));
    }
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(|_| offline())?;
    let _ = request.headers().set("Accept", "application/json");
    if body.is_some() {
        let _ = request.headers().set("Content-Type", "application/json");
    }
    if method != "GET" {
        let _ = request.headers().set(REQUEST_HEADER, "1");
    }
    let resp: web_sys::Response =
        JsFuture::from(window.fetch_with_request(&request)).await.map_err(|_| offline())?.dyn_into().map_err(|_| offline())?;
    let text = JsFuture::from(resp.text().map_err(|_| offline())?).await.map_err(|_| offline())?.as_string().unwrap_or_default();
    // Big enough for every release's notes; anything bigger isn't the launcher server.
    if text.len() > 8 << 20 {
        return Err("the launcher server's answer is too big".into());
    }
    Ok((resp.status(), text))
}
