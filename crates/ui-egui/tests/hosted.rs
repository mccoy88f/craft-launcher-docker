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

fn harness(admin: bool) -> (Harness<'static, Option<LauncherUi>>, Rc<RefCell<Vec<Command>>>) {
    artcraft_launcher_ui_egui::i18n::set_test_lang(artcraft_launcher_ui_egui::i18n::Lang::EN);
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut backend = Some(Hosted { events: events(), sent: sent.clone(), admin });
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
