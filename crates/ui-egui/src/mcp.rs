//! A launcher server's MCP server, for its administrator: the switch on each app's details page,
//! the MCP section of Settings, and the dialogs that make an access token and show it once.
//!
//! Nothing here shows unless the backend sent an [`McpState`] (a launcher server, to the page
//! signed in as its administrator).

use artcraft_launcher_core::hosted::{McpChange, McpFiles, McpState, PREVIOUS_FILES};
use artcraft_launcher_core::protocol::Command;
use egui::{RichText, Ui};

use crate::app::{Dialog, LauncherUi};
use crate::i18n::{fmt, tn};
use crate::model::AppEntry;
use crate::theme::Tokens;
use crate::widgets::{self, Kind};

/// The choices for deleting old files from a work folder, in days (0: never).
const RETENTION_DAYS: [u32; 7] = [0, 1, 3, 7, 14, 30, 90];
/// The choices for the space each client's work folder may use.
const QUOTAS: [u64; 7] = [512 << 20, 1 << 30, 2 << 30, 5 << 30, 10 << 30, 20 << 30, 50 << 30];
const SESSIONS: [u32; 6] = [1, 2, 4, 8, 10, 16];

/// What the new-token dialog is filling in.
#[derive(Clone, Debug, Default)]
pub(crate) struct TokenForm {
    pub name: String,
    /// One app, or `None` for every app that's on.
    pub app: Option<String>,
}

fn retention_label(days: u32) -> String {
    if days == 0 { tl!("Never").to_string() } else { tn(u64::from(days), "{n} day", "{n} days") }
}

impl LauncherUi {
    fn mcp_send(&mut self, change: McpChange) {
        self.send(Command::Mcp(change));
    }

    /// Copies `text` and says so.
    fn copy(&mut self, ui: &Ui, text: String) {
        ui.ctx().copy_text(text);
        let ctx = ui.ctx().clone();
        self.toast(&ctx, tl!("Copied to the clipboard"), false);
    }

    /// The MCP box under an app's Website, GitHub and Releases buttons: one button-like row,
    /// folded by default, that unfolds into the app's MCP status and settings. Everyone sees it;
    /// changing anything asks a visitor to sign in first (the backend takes them to sign in).
    pub(crate) fn mcp_app_section(&mut self, ui: &mut Ui, entry: &AppEntry, width: f32, t: &Tokens) {
        let Some(mcp) = self.mcp.clone() else { return };
        let Some(app) = mcp.app(entry.app.id).cloned() else { return };
        let label = format!("{} {}", tl!("MCP server"), if self.mcp_open { "↑" } else { "↓" });
        ui.add_space(6.0);
        if widgets::button(ui, &label, Kind::Secondary, width).clicked() {
            self.mcp_open = !self.mcp_open;
        }
        if !self.mcp_open {
            return;
        }
        let address = format!("{}{}", mcp.base_url, app.id);
        // The frame adds its 14 px margins and 1 px line on each side: the inside is what's left.
        let inner = (width - 30.0).max(120.0);
        egui::Frame::new().stroke(t.hairline()).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
            ui.set_width(inner);
            ui.set_max_width(inner);
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new(fmt(tl!("Control {name} from Claude or other MCP clients"), &[("name", entry.app.name)])).color(t.muted));
            // Where things stand: the browser version, its MCP tool, and whether MCP is on.
            let browser = match entry.installed.as_ref().and_then(|i| i.version.as_ref()) {
                Some(v) => fmt(tl!("Browser version {version}"), &[("version", &v.to_string())]),
                None => tl!("Not installed").to_string(),
            };
            let tool = if app.ready { tl!("MCP tool ready") } else { tl!("MCP tool not installed") };
            let state = if app.enabled && mcp.enabled { tl!("MCP on") } else { tl!("MCP off") };
            ui.label(RichText::new(format!("{browser} · {tool} · {state}")).color(t.ink_strong));
            if entry.installed.is_some() {
                let mut on = app.enabled;
                if ui.checkbox(&mut on, fmt(tl!("MCP server for {name}"), &[("name", entry.app.name)])).changed() {
                    self.mcp_send(McpChange::App { app: app.id.clone(), on });
                }
            }
            if app.enabled && !mcp.enabled {
                ui.label(RichText::new(tl!("Turn on the MCP server in Settings to reach it.")).size(12.5).color(t.warn));
            } else if app.enabled && !app.ready && entry.job.is_none() {
                ui.label(RichText::new(tl!("Its command-line tool isn't installed yet.")).size(12.5).color(t.warn));
            }
            widgets::hud(ui, tl!("Address"), t.faint);
            // The address gets what the Copy button leaves, cut short with "…" (the button copies it
            // whole), so a long domain never pushes the button out of the box.
            let copy_w = widgets::button_width(ui, tl!("Copy"), 0.0);
            let gap = ui.spacing().item_spacing.x;
            ui.horizontal(|ui| {
                let label_w = (inner - copy_w - gap).max(40.0);
                ui.allocate_ui_with_layout(egui::vec2(label_w, crate::theme::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&address).monospace().color(t.ink_strong)).truncate()).on_hover_text(address.as_str());
                });
                if widgets::button(ui, tl!("Copy"), Kind::Secondary, 0.0).clicked() {
                    self.copy(ui, address.clone());
                }
            });
            ui.columns(2, |cols| {
                widgets::hud(&mut cols[0], tl!("Work folder"), t.faint);
                cols[0].label(RichText::new(&app.work_dir).monospace().size(12.5));
                widgets::hud(&mut cols[1], tl!("Space used"), t.faint);
                cols[1].label(RichText::new(widgets::human_bytes(app.used_bytes)).size(12.5));
            });
            ui.add(
                egui::Label::new(
                    RichText::new(tl!(
                        "Each client (token) has its own folder in it: it works on the files there, not on documents open in visitors' browsers."
                    ))
                    .size(12.0)
                    .color(t.muted),
                )
                .wrap(),
            );
            if self.backend.signed_in() == Some(false) {
                ui.label(RichText::new(tl!("Sign in to change these.")).size(12.0).color(t.muted));
            } else {
                self.mcp_files(ui, &mcp, &app.id, inner, t);
            }
        });
    }

    /// The administrator's view of an app's work folder: each client's files, to download,
    /// delete, or add to.
    fn mcp_files(&mut self, ui: &mut Ui, mcp: &McpState, app: &str, inner: f32, t: &Tokens) {
        let groups: Vec<&McpFiles> = mcp.files.iter().filter(|g| g.app == app).collect();
        // Clients with access but no folder yet can still be given files.
        let mut empty: Vec<McpFiles> = Vec::new();
        for token in mcp.tokens.iter().filter(|tk| tk.apps.as_ref().is_none_or(|l| l.iter().any(|a| a == app))) {
            if !groups.iter().any(|g| g.client == token.id) {
                empty.push(McpFiles { app: app.to_string(), client: token.id.clone(), name: token.name.clone(), ..McpFiles::default() });
            }
        }
        ui.add_space(4.0);
        widgets::hud(ui, tl!("Files"), t.faint);
        if groups.is_empty() && empty.is_empty() {
            ui.label(RichText::new(tl!("No files yet.")).size(12.5).color(t.muted));
            return;
        }
        let now = self.backend.now();
        let gap = ui.spacing().item_spacing.x;
        for group in groups.into_iter().chain(empty.iter()) {
            let who = if group.client == PREVIOUS_FILES {
                tl!("Earlier files (from before each client had its own folder)").to_string()
            } else if group.name.is_empty() {
                tl!("A revoked token").to_string()
            } else {
                group.name.clone()
            };
            let upload = group.client != PREVIOUS_FILES && !group.name.is_empty();
            let upload_w = if upload { widgets::button_width(ui, tl!("Add files…"), 0.0) + gap } else { 0.0 };
            ui.horizontal(|ui| {
                let title = format!("{who} · {}", widgets::human_bytes(group.used_bytes));
                ui.allocate_ui_with_layout(
                    egui::vec2((inner - upload_w).max(40.0), crate::theme::CONTROL_H),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(RichText::new(&title).color(t.ink_strong)).truncate()).on_hover_text(title.as_str());
                    },
                );
                if upload && widgets::button(ui, tl!("Add files…"), Kind::Secondary, 0.0).clicked() {
                    self.send(Command::McpUpload { app: app.to_string(), client: group.client.clone() });
                }
            });
            for file in &group.files {
                let key = format!("file:{app}/{}/{}", group.client, file.path);
                let asking = self.mcp_confirm.as_deref() == Some(key.as_str());
                let (first, second) = if asking { (tl!("Delete"), tl!("Cancel")) } else { (tl!("Download"), tl!("Delete")) };
                let buttons = widgets::button_width(ui, first, 0.0) + widgets::button_width(ui, second, 0.0) + 2.0 * gap;
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(egui::vec2((inner - buttons).max(40.0), 34.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(egui::Label::new(RichText::new(&file.path).monospace().size(12.5).color(t.ink_strong)).truncate())
                            .on_hover_text(file.path.as_str());
                        let when = match file.expires {
                            _ if asking => tl!("Delete this file?").to_string(),
                            Some(at) if at > now => {
                                tn(u64::try_from((at - now + 86_399) / 86_400).unwrap_or(1), "deleted in {n} day", "deleted in {n} days")
                            }
                            Some(_) => tl!("deleted at the next clean-up").to_string(),
                            None => tl!("kept").to_string(),
                        };
                        ui.label(RichText::new(format!("{} · {when}", widgets::human_bytes(file.bytes))).size(11.5).color(if asking {
                            t.warn
                        } else {
                            t.muted
                        }));
                    });
                    if asking {
                        if widgets::button(ui, tl!("Delete"), Kind::Danger, 0.0).clicked() {
                            self.mcp_confirm = None;
                            self.mcp_send(McpChange::DeleteFile { app: app.to_string(), client: group.client.clone(), path: file.path.clone() });
                        }
                        if widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0).clicked() {
                            self.mcp_confirm = None;
                        }
                    } else {
                        if widgets::button(ui, tl!("Download"), Kind::Secondary, 0.0).clicked() {
                            self.send(Command::McpDownload { app: app.to_string(), client: group.client.clone(), path: file.path.clone() });
                        }
                        if widgets::button(ui, tl!("Delete"), Kind::Secondary, 0.0).clicked() {
                            self.mcp_confirm = Some(key.clone());
                        }
                    }
                });
            }
            if group.more > 0 {
                ui.label(RichText::new(tn(u64::from(group.more), "…and {n} more file", "…and {n} more files")).size(12.0).color(t.muted));
            }
        }
    }

    /// The account row at the top of Settings, on a launcher server.
    pub(crate) fn account_settings(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(signed_in) = self.backend.signed_in() else { return };
        ui.horizontal(|ui| {
            if signed_in {
                ui.label(RichText::new(tl!("Signed in as administrator")).color(t.ink_strong));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, tl!("Sign out"), Kind::Secondary, 0.0).clicked() {
                        self.send(Command::SignOut);
                    }
                });
            } else if widgets::button(ui, tl!("Sign in as administrator"), Kind::Primary, 0.0).clicked() {
                self.send(Command::SignIn);
            }
        });
        ui.add_space(6.0);
    }

    /// What the server has installed: each app's browser version and its MCP tool.
    pub(crate) fn server_status(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(mcp) = self.mcp.clone() else { return };
        ui.add_space(14.0);
        widgets::hud(ui, tl!("On this server"), t.faint);
        ui.add_space(6.0);
        let installed: Vec<_> = self.entries.iter().filter(|e| e.installed.is_some()).cloned().collect();
        if installed.is_empty() {
            ui.label(RichText::new(tl!("Not installed")).color(t.muted));
        }
        for e in installed {
            let version = e.installed.as_ref().and_then(|i| i.version.as_ref()).map(ToString::to_string).unwrap_or_default();
            let app = mcp.app(e.app.id);
            let tool = if app.is_some_and(|a| a.ready) { tl!("MCP tool ready") } else { tl!("MCP tool not installed") };
            let state = if mcp.enabled && app.is_some_and(|a| a.enabled) { tl!("MCP on") } else { tl!("MCP off") };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(e.app.name).color(t.ink_strong));
                ui.label(
                    RichText::new(format!("{} · {tool} · {state}", fmt(tl!("Browser version {version}"), &[("version", &version)])))
                        .size(12.5)
                        .color(t.muted),
                );
            });
        }
    }

    /// The MCP section of Settings.
    pub(crate) fn mcp_settings(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(mcp) = self.mcp.clone() else { return };
        ui.add_space(14.0);
        widgets::hud(ui, tl!("MCP server"), t.faint);
        ui.add_space(6.0);
        if self.backend.signed_in() == Some(false) {
            ui.label(RichText::new(tl!("Sign in to change these.")).size(12.0).color(t.muted));
        }
        let mut on = mcp.enabled;
        if ui.checkbox(&mut on, tl!("Turn on the MCP server")).changed() {
            self.mcp_send(McpChange::Enabled { on });
        }
        ui.label(RichText::new(tl!("When off, no app can be reached, whatever its own switch says.")).size(12.0).color(t.muted));
        ui.add_space(8.0);

        widgets::hud(ui, tl!("Apps on MCP"), t.faint);
        let on_apps: Vec<_> = mcp.apps.iter().filter(|a| a.enabled).cloned().collect();
        if on_apps.is_empty() {
            ui.label(RichText::new(tl!("No app is on yet.")).color(t.muted));
        }
        for app in &on_apps {
            let name = artcraft_launcher_core::CraftApp::by_id(&app.id).map_or(app.id.as_str(), |a| a.name);
            ui.horizontal(|ui| {
                ui.label(RichText::new(name).color(t.ink_strong));
                ui.label(RichText::new(widgets::human_bytes(app.used_bytes)).color(t.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, tl!("Empty"), Kind::Secondary, 0.0).clicked() {
                        self.mcp_send(McpChange::Empty { app: app.id.clone() });
                    }
                });
            });
        }
        ui.label(RichText::new(tl!("Turn an app on or off in its details.")).size(12.0).color(t.muted));
        ui.add_space(8.0);

        let mut days = mcp.retention_days;
        ui.horizontal(|ui| {
            ui.label(tl!("Delete files not changed for"));
            egui::ComboBox::from_id_salt("mcp-retention").selected_text(retention_label(days)).width(120.0).show_ui(ui, |ui| {
                for d in RETENTION_DAYS {
                    ui.selectable_value(&mut days, d, retention_label(d));
                }
            });
        });
        if days != mcp.retention_days {
            self.mcp_send(McpChange::RetentionDays { days });
        }
        let mut quota = mcp.quota_bytes;
        ui.horizontal(|ui| {
            ui.label(tl!("Space per client"));
            egui::ComboBox::from_id_salt("mcp-quota").selected_text(widgets::human_bytes(quota)).width(120.0).show_ui(ui, |ui| {
                for q in QUOTAS {
                    ui.selectable_value(&mut quota, q, widgets::human_bytes(q));
                }
            });
        });
        if quota != mcp.quota_bytes {
            self.mcp_send(McpChange::QuotaBytes { bytes: quota });
        }
        let mut sessions = mcp.max_sessions;
        ui.horizontal(|ui| {
            ui.label(tl!("Sessions at once"));
            egui::ComboBox::from_id_salt("mcp-sessions").selected_text(sessions.to_string()).width(80.0).show_ui(ui, |ui| {
                for s in SESSIONS {
                    ui.selectable_value(&mut sessions, s, s.to_string());
                }
            });
        });
        if sessions != mcp.max_sessions {
            self.mcp_send(McpChange::MaxSessions { sessions });
        }
        let mut from_url = mcp.file_from_url;
        if ui.checkbox(&mut from_url, tl!("Let MCP clients download files from web addresses")).changed() {
            self.mcp_send(McpChange::FileFromUrl { on: from_url });
        }
        ui.add(
            egui::Label::new(
                RichText::new(tl!(
                    "Adds the launcher_file_from_url tool: the server fetches a public https address into the client's folder. Off unless you need it."
                ))
                .size(12.0)
                .color(t.muted),
            )
            .wrap(),
        );
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            widgets::hud(ui, tl!("Access tokens"), t.faint);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(ui, tl!("New token"), Kind::Primary, 0.0).clicked() {
                    self.token_form = TokenForm::default();
                    self.dialog = Some(Dialog::NewToken);
                }
            });
        });
        if mcp.tokens.is_empty() {
            ui.label(RichText::new(tl!("No tokens yet.")).color(t.muted));
        }
        let now = self.backend.now();
        for token in &mcp.tokens {
            egui::Frame::new().stroke(t.hairline()).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(&token.name).color(t.ink_strong));
                        let apps = match &token.apps {
                            None => tl!("All apps").to_string(),
                            Some(list) => list
                                .iter()
                                .map(|id| artcraft_launcher_core::CraftApp::by_id(id).map_or(id.as_str(), |a| a.name))
                                .collect::<Vec<_>>()
                                .join(", "),
                        };
                        let used = match token.last_used {
                            Some(at) => fmt(tl!("Used {ago}"), &[("ago", &crate::views::ago_text(at, now))]),
                            None => tl!("Not used yet").to_string(),
                        };
                        // "OAuth": a client that signed in (claude.ai, Claude's apps); a name in every language.
                        let how = if token.oauth { " · OAuth" } else { "" };
                        ui.label(RichText::new(format!("…{} · {apps} · {used}{how}", token.hint)).size(12.0).color(t.muted));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let key = format!("revoke:{}", token.id);
                        if self.mcp_confirm.as_deref() == Some(key.as_str()) {
                            if widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0).clicked() {
                                self.mcp_confirm = None;
                            }
                            if widgets::button(ui, tl!("Revoke"), Kind::Danger, 0.0).clicked() {
                                self.mcp_confirm = None;
                                self.mcp_send(McpChange::RevokeToken { id: token.id.clone() });
                            }
                        } else if widgets::button(ui, tl!("Revoke"), Kind::Danger, 0.0).clicked() {
                            // A visitor goes to sign in; the administrator is asked once more.
                            if self.backend.signed_in() == Some(false) {
                                self.mcp_send(McpChange::RevokeToken { id: token.id.clone() });
                            } else {
                                self.mcp_confirm = Some(key);
                            }
                        }
                    });
                });
                if self.mcp_confirm.as_deref() == Some(format!("revoke:{}", token.id).as_str()) {
                    ui.label(RichText::new(tl!("Revoke it? Its files on the server are deleted too.")).size(12.0).color(t.warn));
                }
            });
        }
        ui.add(
            egui::Label::new(
                RichText::new(tl!(
                    "A token is shown only once, when it's made; the server keeps only its fingerprint. Give each client its own token, so you can revoke one without touching the others."
                ))
                .size(12.0)
                .color(t.muted),
            )
            .wrap(),
        );
    }

    /// Names a new token and picks what it may use.
    pub(crate) fn new_token_dialog(&mut self, ui: &mut Ui) -> bool {
        let Some(mcp) = self.mcp.clone() else { return true };
        ui.set_width(440.0f32.min(ui.ctx().content_rect().width() - 80.0));
        ui.label(widgets::heading(tl!("New MCP token"), 22.0, Tokens::get(ui.ctx()).ink_strong));
        ui.add_space(12.0);
        ui.label(tl!("Name"));
        ui.add(egui::TextEdit::singleline(&mut self.token_form.name).desired_width(f32::INFINITY).char_limit(80));
        ui.add_space(8.0);
        ui.label(tl!("Allowed apps"));
        let label = |id: &Option<String>| match id {
            None => tl!("All apps").to_string(),
            Some(id) => artcraft_launcher_core::CraftApp::by_id(id).map_or_else(|| id.clone(), |a| a.name.to_string()),
        };
        let mut choice = self.token_form.app.clone();
        egui::ComboBox::from_id_salt("token-apps").selected_text(label(&choice)).width(240.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut choice, None, label(&None));
            for app in mcp.apps.iter().filter(|a| a.enabled) {
                let id = Some(app.id.clone());
                ui.selectable_value(&mut choice, id.clone(), label(&id));
            }
        });
        self.token_form.app = choice;
        ui.add_space(18.0);
        let mut close = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let name = self.token_form.name.trim().to_string();
                if ui.add_enabled_ui(!name.is_empty(), |ui| widgets::button(ui, tl!("Create"), Kind::Primary, 100.0)).inner.clicked() {
                    let apps = self.token_form.app.clone().map(|a| vec![a]);
                    self.mcp_send(McpChange::CreateToken { name, apps });
                    close = true;
                }
                if widgets::button(ui, tl!("Cancel"), Kind::Secondary, 90.0).clicked() {
                    close = true;
                }
            });
        });
        close
    }

    /// The new token, once, with what to paste into an MCP client.
    pub(crate) fn token_created_dialog(&mut self, ui: &mut Ui, token: &str) -> bool {
        let t = Tokens::get(ui.ctx());
        ui.set_width(520.0f32.min(ui.ctx().content_rect().width() - 80.0));
        ui.label(widgets::heading(tl!("New MCP token"), 22.0, t.ink_strong));
        ui.label(RichText::new(tl!("Copy it now: it won't be shown again.")).color(t.warn));
        ui.add_space(12.0);
        widgets::hud(ui, tl!("Token"), t.faint);
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(token).monospace().color(t.ink_strong)).truncate());
            if widgets::button(ui, tl!("Copy"), Kind::Secondary, 0.0).clicked() {
                self.copy(ui, token.to_string());
            }
        });
        ui.add_space(8.0);
        let base = self.mcp.as_ref().map(|m| m.base_url.clone()).unwrap_or_default();
        let first = self.mcp.as_ref().and_then(|m| m.apps.iter().find(|a| a.enabled)).map_or("photocraft", |a| a.id.as_str()).to_string();
        let command = format!("claude mcp add --transport http {first} {base}{first} --header \"Authorization: Bearer {token}\"");
        widgets::hud(ui, tl!("For Claude Code"), t.faint);
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(&command).monospace().size(12.0)).wrap());
        });
        if widgets::button(ui, tl!("Copy"), Kind::Secondary, 0.0).clicked() {
            self.copy(ui, command.clone());
        }
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(
                RichText::new(tl!(
                    "For other MCP clients, add a remote server with the app's address and the header Authorization: Bearer followed by this token."
                ))
                .size(12.5)
                .color(t.muted),
            )
            .wrap(),
        );
        ui.add_space(16.0);
        let mut close = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = widgets::button(ui, tl!("I've copied the token"), Kind::Primary, 0.0).clicked();
            });
        });
        close
    }
}

/// Whether the MCP server of `app` is on, for the badge on its card.
pub(crate) fn badge(mcp: Option<&McpState>, app: &str) -> bool {
    mcp.is_some_and(|m| m.enabled && m.app(app).is_some_and(|a| a.enabled && a.ready))
}
