//! The launcher as a launcher server serves it (`artcraft-launcher-server`): the apps are browser
//! builds, visitors only open the installed ones, and the administrator installs and updates them.

use std::cell::RefCell;
use std::rc::Rc;

use artcraft_launcher_core::protocol::{Backend, Capabilities, Command, Event, LatestRelease, Origin, Source};
use artcraft_launcher_core::release::{Asset, Release};
use artcraft_launcher_core::status::InstallSource;
use artcraft_launcher_core::{Arch, Installed, Os, Platform, SUITE, version};
use artcraft_launcher_ui_egui::settings::ViewMode;
use artcraft_launcher_ui_egui::{Host, LauncherUi, Settings};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

const NOW: i64 = 1_791_500_000;

struct Hosted {
    events: Vec<Event>,
    sent: Rc<RefCell<Vec<Command>>>,
    admin: bool,
}

impl Backend for Hosted {
    fn send(&mut self, command: Command) {
        self.sent.borrow_mut().push(command);
    }
    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { manage_apps: true, reveal_label: "", self_update: false, install_apps: self.admin, web_apps: true }
    }
    fn platform(&self) -> Option<Platform> {
        Some(Platform { os: Os::Linux, arch: Arch::X64 })
    }
    fn now(&self) -> i64 {
        NOW
    }
}

struct NoHost;
impl Host for NoHost {}

/// A release with only a browser build, or (`web: false`) only a desktop one.
fn release(id: &str, v: &str, web: bool) -> Release {
    let name = if web { format!("{id}-web-{v}.zip") } else { format!("{id}-{v}-linux-x86_64.AppImage") };
    Release {
        tag: format!("v{v}"),
        version: version::parse(v),
        name: format!("v{v}"),
        published_at: Some(NOW - 86_400),
        html_url: format!("https://github.com/storytold/{id}/releases/tag/v{v}"),
        notes: String::new(),
        prerelease: false,
        assets: vec![Asset {
            name: name.clone(),
            size: 12 << 20,
            url: format!("https://github.com/storytold/{id}/releases/download/v{v}/{name}"),
            sha256: None,
        }],
    }
}

fn events() -> Vec<Event> {
    let mut ev = Vec::new();
    for (id, v, web) in [("photocraft", "0.8.2", true), ("vectorcraft", "0.7.0", true), ("filmcraft", "0.4.0", false), ("effectcraft", "0.6.0", true)]
    {
        ev.push(Event::Latest { app: id.into(), latest: LatestRelease::Found(release(id, v, web)), from: Origin::GitHub });
    }
    let installed = [("photocraft", "0.8.2"), ("effectcraft", "0.5.0")];
    for app in &SUITE {
        let inst = installed.iter().find(|(id, _)| *id == app.id).map(|(_, v)| Installed {
            version: version::parse(v),
            source: InstallSource::Launcher,
            location: format!("apps/{}/", app.id),
            package: None,
        });
        ev.push(Event::Detected { app: app.id.into(), installed: inst });
    }
    ev.push(Event::CheckFinished { at: NOW - 60, source: Source::GitHubApi, rate_limited_until: None });
    ev
}

/// The MCP server as an administrator's page gets it: PhotoCraft on, one token.
fn mcp_event() -> Event {
    use artcraft_launcher_core::hosted::{McpApp, McpState, McpToken};
    Event::Mcp(McpState {
        enabled: true,
        retention_days: 7,
        quota_bytes: 2 << 30,
        max_sessions: 4,
        base_url: "https://launcher.example.com/mcp/".into(),
        apps: SUITE
            .iter()
            .map(|a| McpApp {
                id: a.id.into(),
                enabled: a.id == "photocraft",
                ready: a.id == "photocraft",
                work_dir: format!("/work/{}", a.id),
                used_bytes: 3 << 20,
                sessions: 0,
            })
            .collect(),
        tokens: vec![McpToken {
            id: "t1".into(),
            name: "Claude Desktop".into(),
            hint: "7f3a".into(),
            apps: None,
            created_at: NOW - 3600,
            last_used: Some(NOW - 300),
        }],
    })
}

fn harness(admin: bool) -> (Harness<'static, Option<LauncherUi>>, Rc<RefCell<Vec<Command>>>) {
    harness_with(admin, events())
}

fn harness_with(admin: bool, events: Vec<Event>) -> (Harness<'static, Option<LauncherUi>>, Rc<RefCell<Vec<Command>>>) {
    artcraft_launcher_ui_egui::i18n::set_test_lang(artcraft_launcher_ui_egui::i18n::Lang::EN);
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut backend = Some(Hosted { events, sent: sent.clone(), admin });
    let settings = Settings { view: ViewMode::Grid, notify: false, ..Settings::default() };
    let mut h = Harness::builder().with_size(egui::vec2(1180.0, 900.0)).build_ui_state(
        move |ui, state: &mut Option<LauncherUi>| {
            if state.is_none()
                && let Some(b) = backend.take()
            {
                *state = Some(LauncherUi::new(ui.ctx(), Box::new(b), settings.clone()));
            }
            if let Some(l) = state.as_mut() {
                l.logic(&ui.ctx().clone(), &mut NoHost);
                l.ui(ui, &mut NoHost);
            }
        },
        None,
    );
    h.run_steps(8);
    (h, sent)
}

#[test]
fn visitors_open_installed_apps_and_install_nothing() {
    let (mut h, sent) = harness(false);
    // Installed apps open, even one with a newer release (only the administrator updates).
    assert_eq!(h.query_all_by_label("Open").count(), 2);
    assert!(h.query_by_label("Install").is_none());
    assert!(h.query_by_label_contains("Update to").is_none());
    assert!(h.query_by_label_contains("Update all").is_none());
    assert!(h.query_by_label("Available to install").is_none());
    // Nothing about the visitor's own system: the apps run in the browser.
    assert!(h.query_by_label_contains("Installing for").is_none());
    assert!(h.query_by_label_contains("Download for").is_none());
    h.query_all_by_label("Open").next().unwrap().click();
    h.run_steps(2);
    assert!(sent.borrow().iter().any(|c| matches!(c, Command::Launch { app } if app == "photocraft")), "{:?}", sent.borrow());
    assert!(!sent.borrow().iter().any(|c| matches!(c, Command::Install { .. })));
}

#[test]
fn the_administrator_installs_and_updates_browser_builds() {
    let (mut h, sent) = harness(true);
    h.get_by_label("Update to 0.6.0");
    h.get_by_label("Update 1 app");
    // VectorCraft has a browser build; FilmCraft's release has only a desktop one.
    assert_eq!(h.query_all_by_label("Install").count(), 1);
    assert!(h.query_all_by_label("Not available").count() >= 1);
    h.get_by_label("Install").click();
    h.run_steps(2);
    assert!(sent.borrow().iter().any(|c| matches!(c, Command::Install { app } if app == "vectorcraft")), "{:?}", sent.borrow());
}

#[test]
fn the_administrator_sees_and_changes_the_mcp_server() {
    let mut events = events();
    events.push(mcp_event());
    let (mut h, sent) = harness_with(true, events);
    // PhotoCraft's card says its MCP server is on.
    assert_eq!(h.query_all_by_label("MCP").count(), 1);
    h.get_by_label("Settings").click();
    h.run_steps(4);
    h.get_by_label("Turn on the MCP server");
    h.get_by_label_contains("Claude Desktop");
    h.get_by_label("Empty").click();
    h.run_steps(2);
    assert!(
        sent.borrow().iter().any(|c| matches!(c, Command::Mcp(artcraft_launcher_core::hosted::McpChange::Empty { app }) if app == "photocraft")),
        "{:?}",
        sent.borrow()
    );
    h.get_by_label("New token").click();
    h.run_steps(4);
    h.get_by_label("New MCP token");
}

#[test]
fn a_new_token_is_shown_once() {
    let mut events = events();
    events.push(mcp_event());
    events.push(Event::McpTokenCreated("acl_secret1234".into()));
    let (mut h, _) = harness_with(true, events);
    h.get_by_label("acl_secret1234");
    h.get_by_label_contains("claude mcp add --transport http photocraft https://launcher.example.com/mcp/photocraft");
    h.get_by_label("I've copied the token").click();
    h.run_steps(4);
    assert!(h.query_by_label("acl_secret1234").is_none());
}
