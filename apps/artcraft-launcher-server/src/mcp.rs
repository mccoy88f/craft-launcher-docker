//! The Craft Apps as MCP servers, over MCP's Streamable HTTP transport at `/mcp/<app>`.
//!
//! Each app's command-line tool (`<app>-cli mcp`) is an MCP server over stdio. A client's
//! `initialize` starts one for its session, with the app's work folder as its only read and write
//! root; later requests carry the `Mcp-Session-Id` the answer gave. Requests are written to the
//! tool's stdin, and its answers are matched back by JSON-RPC id; what it sends meanwhile
//! (progress, logs) reaches the client too when it accepts an event stream.
//!
//! Only the administrator turns the MCP server and each app's on, and makes the access tokens
//! clients sign in with (stored as SHA-256 fingerprints, shown once). Each client (a token, or an
//! OAuth client's grant) has its own work folder in each app, `<work>/<app>/<token id>`: its
//! sessions share it, and no other client sees it. Work folders are emptied of files not changed
//! for `retention_days`, and a tool call is refused while the client's folder holds more than the
//! quota.
//!
//! The launcher adds tools of its own to every app's (`launcher_*`): list the client's files, give
//! a link to download one, and (when the administrator allows it) fetch one from a web address.
//! The answer to `initialize` tells the client how long files are kept and which expire soon.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::catalog::{CraftApp, SUITE};
use artcraft_launcher_core::hosted::{McpApp, McpFile, McpFiles, McpState, McpToken, PREVIOUS_FILES};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::hosting::Hosting;

/// A session nobody used for this long is closed.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// The longest a request may wait for its answer, unless the client takes an event stream.
const ANSWER_WAIT: Duration = Duration::from_secs(15 * 60);
/// One message from a tool may not be bigger than this.
const MAX_LINE: usize = 64 << 20;
/// How long a folder's measured size is trusted.
const SIZE_FRESH: Duration = Duration::from_secs(30);
/// What a token starts with, so it's easy to recognise (and to find if it leaks).
const TOKEN_PREFIX: &str = "acl_";

pub const RETENTION_CHOICES: [u32; 7] = [0, 1, 3, 7, 14, 30, 90];
/// How long a download link a client asked for works.
const LINK_LIFE: Duration = Duration::from_secs(3600);
/// The most files listed per folder, on the page and to a client.
const LIST_MAX: usize = 200;
/// The biggest file `launcher_file_from_url` fetches, or the administrator uploads.
pub const FILE_MAX: u64 = 512 << 20;
/// Files expiring within this long are named in the answer to `initialize`.
const EXPIRING_SOON: i64 = 2 * 24 * 3600;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredToken {
    id: String,
    name: String,
    /// Hex SHA-256 of the whole token.
    hash: String,
    hint: String,
    apps: Option<Vec<String>>,
    created_at: i64,
    last_used: Option<i64>,
    /// Set for a token an OAuth client got (claude.ai, Claude's apps): it expires and is renewed.
    #[serde(default)]
    grant: Option<Grant>,
}

/// What an OAuth client was granted: its access token expires within the hour, and its refresh
/// token (rotated on every use) gets it a new one.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Grant {
    client_id: String,
    access_expires: i64,
    refresh_hash: String,
    refresh_expires: i64,
}

/// An OAuth client that registered itself (RFC 7591).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    /// Hex SHA-256 of its secret, for a confidential client.
    #[serde(default)]
    secret_hash: Option<String>,
    pub created_at: i64,
}

/// A client as the authorization endpoint needs it: registered here, or described by its
/// metadata document (its `client_id` is that document's https URL).
#[derive(Clone, Debug)]
pub struct ClientInfo {
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    secret_hash: Option<String>,
}

/// A code the authorization endpoint gave, redeemed once at the token endpoint.
struct AuthCode {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    challenge: String,
    apps: Option<Vec<String>>,
    expires: Instant,
}

/// What the token endpoint hands out.
#[derive(Debug, Serialize)]
pub struct TokenSet {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub refresh_token: String,
}

/// An OAuth error code (RFC 6749 §5.2).
pub type OAuthError = &'static str;

const ACCESS_LIFE: i64 = 3600;
const REFRESH_LIFE: i64 = 60 * 24 * 3600;
const CODE_LIFE: Duration = Duration::from_secs(600);
const REFRESH_PREFIX: &str = "aclr_";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_retention")]
    retention_days: u32,
    #[serde(default = "default_quota")]
    quota_bytes: u64,
    #[serde(default = "default_sessions")]
    max_sessions: u32,
    #[serde(default)]
    apps: BTreeSet<String>,
    #[serde(default)]
    tokens: Vec<StoredToken>,
    #[serde(default)]
    clients: Vec<OAuthClient>,
    /// Unix seconds of the last clean-up of old files.
    #[serde(default)]
    last_cleanup: i64,
    #[serde(default)]
    file_from_url: bool,
    /// 1 once the files from before per-client folders were moved to [`PREVIOUS_FILES`]; 2 once
    /// sessions at once left at the old default (4) were raised to the new one.
    #[serde(default)]
    layout: u32,
}

fn default_retention() -> u32 {
    7
}
fn default_quota() -> u64 {
    2 << 30
}
fn default_sessions() -> u32 {
    10
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: default_retention(),
            quota_bytes: default_quota(),
            max_sessions: default_sessions(),
            apps: BTreeSet::new(),
            tokens: Vec::new(),
            clients: Vec::new(),
            last_cleanup: 0,
            file_from_url: false,
            layout: 2,
        }
    }
}

/// One running tool.
pub struct Session {
    pub id: String,
    app: &'static str,
    token: String,
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    child: Mutex<Option<tokio::process::Child>>,
    /// Requests waiting for an answer, by JSON-RPC id, each with where its messages go.
    pending: Mutex<HashMap<String, mpsc::UnboundedSender<String>>>,
    last_used: Mutex<Instant>,
    closed: AtomicBool,
}

impl Session {
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle(&self) -> Duration {
        self.last_used.lock().unwrap_or_else(PoisonError::into_inner).elapsed()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = child.start_kill();
        }
        // Dropping the senders ends every stream still waiting.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// Sends one message to the tool. With an id, returns where its answer (and anything the tool
    /// says meanwhile) arrives; the channel closes after the answer.
    pub async fn send(&self, message: &Value) -> Result<Option<mpsc::UnboundedReceiver<String>>, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("the session has ended".into());
        }
        self.touch();
        let rx = match message.get("id").filter(|_| message.get("method").is_some()) {
            Some(id) => {
                let (tx, rx) = mpsc::unbounded_channel();
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id.to_string(), tx);
                Some(rx)
            }
            None => None,
        };
        let mut line = serde_json::to_vec(message).map_err(|e| format!("unreadable message ({e})"))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await.map_err(|e| format!("the app's MCP server stopped ({e})"))?;
        stdin.flush().await.map_err(|e| format!("the app's MCP server stopped ({e})"))?;
        Ok(rx)
    }
}

/// Why a request was turned away.
pub enum Refusal {
    /// No valid token: 401.
    SignIn,
    /// This token may not use this app: 403.
    Forbidden,
    /// The MCP server or this app's is off, or the app isn't installed: 404.
    Off,
}

pub struct Mcp {
    settings_path: PathBuf,
    work_root: PathBuf,
    settings: Mutex<Settings>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sizes: Mutex<HashMap<PathBuf, (u64, Instant)>>,
    /// Whether each command-line tool takes `--automation-read-root` (asked once per file).
    confinable: Mutex<HashMap<PathBuf, bool>>,
    /// Download links handed to clients: the file, and until when.
    links: Mutex<HashMap<String, (PathBuf, Instant)>>,
    dirty: AtomicBool,
    codes: Mutex<HashMap<String, AuthCode>>,
    /// Client metadata documents read lately, by client id.
    documents: Mutex<HashMap<String, (ClientInfo, Instant)>>,
}

impl Mcp {
    pub fn open(data: &Path, work_root: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&work_root).map_err(|e| format!("couldn't create {} ({e})", work_root.display()))?;
        let settings_path = data.join("mcp.json");
        let saved = std::fs::read(&settings_path).ok();
        // Settings saved before `layout` existed come in with 0 (its serde default); a new
        // installation starts at 1.
        let settings: Settings = saved.as_deref().and_then(|b| serde_json::from_slice(b).ok()).unwrap_or_default();
        let this = Self {
            settings_path,
            work_root,
            settings: Mutex::new(settings),
            sessions: Mutex::default(),
            sizes: Mutex::default(),
            links: Mutex::default(),
            confinable: Mutex::default(),
            dirty: AtomicBool::new(false),
            codes: Mutex::default(),
            documents: Mutex::default(),
        };
        if this.settings().layout < 1 {
            this.move_to_previous();
            this.settings().layout = 1;
            this.save();
        }
        if this.settings().layout < 2 {
            let mut s = this.settings();
            // 4 was the default: clients like claude.ai open several sessions, so it ran out.
            // A number the administrator chose stays.
            if s.max_sessions == 4 {
                s.max_sessions = default_sessions();
            }
            s.layout = 2;
            drop(s);
            this.save();
        }
        Ok(this)
    }

    /// Files from when every client shared the app's folder go to [`PREVIOUS_FILES`], which the
    /// administrator can download and no client sees.
    fn move_to_previous(&self) {
        for app in &SUITE {
            let dir = self.work_dir(app.id);
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let previous = dir.join(PREVIOUS_FILES);
            for e in entries.flatten() {
                if e.file_name() == PREVIOUS_FILES {
                    continue;
                }
                if let Err(err) = std::fs::create_dir_all(&previous).and_then(|()| std::fs::rename(e.path(), previous.join(e.file_name()))) {
                    log::warn!("couldn't move {} to {} ({err})", e.path().display(), previous.display());
                }
            }
        }
    }

    fn settings(&self) -> std::sync::MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(&self) {
        let json = match serde_json::to_vec_pretty(&*self.settings()) {
            Ok(j) => j,
            Err(e) => return log::warn!("couldn't save the MCP settings ({e})"),
        };
        if let Err(e) = artcraft_launcher_native::paths::write_atomic(&self.settings_path, &json) {
            log::warn!("couldn't save the MCP settings ({e})");
        }
        self.dirty.store(false, Ordering::SeqCst);
    }

    pub fn work_dir(&self, app: &str) -> PathBuf {
        self.work_root.join(app)
    }

    /// A client's work folder in an app. `client` is a token's id (URL-safe base64) or
    /// [`PREVIOUS_FILES`].
    pub fn client_dir(&self, app: &str, client: &str) -> Option<PathBuf> {
        let plain = !client.is_empty() && client.len() <= 32 && client.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        plain.then(|| self.work_dir(app).join(client))
    }

    /// A file in a client's folder, from a path relative to it; `None` for a path reaching out of
    /// it (`..`, absolute paths, links).
    pub fn file_path(&self, app: &str, client: &str, path: &str) -> Option<PathBuf> {
        let dir = self.client_dir(app, client)?;
        let path = path.trim_start_matches("./");
        let relative = Path::new(path);
        let plain = !path.is_empty()
            && path.len() <= 1024
            && relative.components().all(|c| matches!(c, std::path::Component::Normal(_)))
            && !path.contains('\\');
        if !plain {
            return None;
        }
        let full = dir.join(relative);
        // Nothing on the way may be a link (to somewhere else).
        let mut at = dir.clone();
        for c in relative.components() {
            at.push(c);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                return None;
            }
        }
        Some(full)
    }

    /// The files in a client's folder, newest first (at most [`LIST_MAX`]), and how many more.
    pub fn files(&self, app: &str, client: &str) -> (Vec<McpFile>, u32) {
        let Some(dir) = self.client_dir(app, client) else { return (Vec::new(), 0) };
        let days = self.settings().retention_days;
        let mut files = Vec::new();
        list_files(&dir, "", &mut files, 0);
        files.sort_by(|a: &McpFile, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
        // The previous files are kept as long as the others.
        if days > 0 {
            for f in &mut files {
                f.expires = Some(f.modified + i64::from(days) * 24 * 3600);
            }
        }
        let more = u32::try_from(files.len().saturating_sub(LIST_MAX)).unwrap_or(u32::MAX);
        files.truncate(LIST_MAX);
        (files, more)
    }

    /// Every client's folder in every app, for the administrator.
    fn all_files(&self, tokens: &[StoredToken]) -> Vec<McpFiles> {
        let mut out = Vec::new();
        for app in &SUITE {
            let Ok(entries) = std::fs::read_dir(self.work_dir(app.id)) else { continue };
            let mut clients: Vec<String> =
                entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).filter_map(|e| e.file_name().into_string().ok()).collect();
            clients.sort();
            for client in clients {
                let Some(dir) = self.client_dir(app.id, &client) else { continue };
                let (files, more) = self.files(app.id, &client);
                let name = tokens.iter().find(|t| t.id == client).map(|t| t.name.clone()).unwrap_or_default();
                out.push(McpFiles { app: app.id.to_string(), client, name, used_bytes: self.used_at(&dir), files, more });
            }
        }
        out
    }

    /// Whether the app's MCP server is on (so updates fetch its tool too).
    pub fn app_on(&self, app: &str) -> bool {
        self.settings().apps.contains(app)
    }

    /// What the page shows; `base_url` is where `/mcp/` is from the page asking. Without `admin`,
    /// tokens are listed without even their last characters.
    pub fn state(&self, hosting: &Hosting, base_url: &str, admin: bool) -> McpState {
        let s = self.settings().clone();
        let sessions: Vec<&'static str> = self.sessions.lock().unwrap_or_else(PoisonError::into_inner).values().map(|x| x.app).collect();
        McpState {
            enabled: s.enabled,
            retention_days: s.retention_days,
            quota_bytes: s.quota_bytes,
            max_sessions: s.max_sessions,
            base_url: base_url.to_string(),
            apps: SUITE
                .iter()
                .map(|a| McpApp {
                    id: a.id.to_string(),
                    enabled: s.apps.contains(a.id),
                    ready: hosting.cli(a.id).is_some(),
                    work_dir: self.work_dir(a.id).display().to_string(),
                    used_bytes: self.used_at(&self.work_dir(a.id)),
                    sessions: u32::try_from(sessions.iter().filter(|x| **x == a.id).count()).unwrap_or(u32::MAX),
                    unconfined: hosting.cli(a.id).is_some_and(|cli| !self.confinable(&cli)),
                })
                .collect(),
            tokens: s
                .tokens
                .iter()
                .map(|t| McpToken {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    hint: if admin { t.hint.clone() } else { "••••".into() },
                    apps: t.apps.clone(),
                    created_at: t.created_at,
                    last_used: t.last_used,
                    oauth: t.grant.is_some(),
                })
                .collect(),
            file_from_url: s.file_from_url,
            files: if admin { self.all_files(&s.tokens) } else { Vec::new() },
        }
    }

    /// Whether a command-line tool can be confined to a work folder: started with
    /// `--automation-read-root` and `--automation-write-root`, it answers `initialize` (releases
    /// from before the options reject them and exit). Tried once per tool; one that can't be
    /// confined is never run, since it could reach every file the server can.
    pub fn confinable(&self, cli: &Path) -> bool {
        if let Some(known) = self.confinable.lock().unwrap_or_else(PoisonError::into_inner).get(cli) {
            return *known;
        }
        let root = self.work_root.join(format!(".probe-{}", random_text(6).unwrap_or_default()));
        let result = answers_confined(cli, &root);
        if let Err(why) = &result {
            log::warn!("{} can't be confined to a work folder, so it won't be run: {why}", cli.display());
        }
        let answer = result.is_ok();
        self.confinable.lock().unwrap_or_else(PoisonError::into_inner).insert(cli.to_path_buf(), answer);
        answer
    }

    /// Bytes in a folder, measured at most every half minute.
    fn used_at(&self, dir: &Path) -> u64 {
        if let Some((bytes, at)) = self.sizes.lock().unwrap_or_else(PoisonError::into_inner).get(dir)
            && at.elapsed() < SIZE_FRESH
        {
            return *bytes;
        }
        let bytes = dir_size(dir);
        self.sizes.lock().unwrap_or_else(PoisonError::into_inner).insert(dir.to_path_buf(), (bytes, Instant::now()));
        bytes
    }

    /// Forgets the measured sizes (something was written or deleted).
    fn forget_sizes(&self) {
        self.sizes.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    pub fn set_file_from_url(&self, on: bool) {
        self.settings().file_from_url = on;
        self.save();
    }

    /// Deletes one file of a client's folder (the administrator).
    pub fn delete_file(&self, app: &CraftApp, client: &str, path: &str) -> Result<(), String> {
        let file = self.file_path(app.id, client, path).ok_or("no such file")?;
        if !file.is_file() {
            return Err("no such file".into());
        }
        std::fs::remove_file(&file).map_err(|e| format!("couldn't delete {path} ({e})"))?;
        self.forget_sizes();
        log::info!("{}: deleted {path} from an MCP work folder", app.name);
        Ok(())
    }

    /// Saves a file into a client's folder (the administrator's upload), replacing one of the
    /// same name; the client's quota still applies.
    pub fn save_file(&self, app: &CraftApp, client: &str, path: &str, bytes: &[u8]) -> Result<(), String> {
        if client == PREVIOUS_FILES || !self.settings().tokens.iter().any(|t| t.id == client) {
            return Err("no such client".into());
        }
        let file = self.file_path(app.id, client, path).ok_or("that file name can't be used")?;
        let quota = self.settings().quota_bytes;
        let dir = self.client_dir(app.id, client).ok_or("no such client")?;
        let replaced = std::fs::metadata(&file).map_or(0, |m| m.len());
        if dir_size(&dir).saturating_sub(replaced) + bytes.len() as u64 > quota {
            return Err(format!("it doesn't fit in the client's space ({} MB)", quota >> 20));
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("couldn't create {} ({e})", parent.display()))?;
        }
        artcraft_launcher_native::paths::write_atomic(&file, bytes).map_err(|e| format!("couldn't save {path} ({e})"))?;
        self.forget_sizes();
        Ok(())
    }

    /// A download link for a file of a client's folder: its id, working for [`LINK_LIFE`].
    fn new_link(&self, file: PathBuf) -> Result<String, String> {
        let id = random_text(24)?;
        let mut links = self.links.lock().unwrap_or_else(PoisonError::into_inner);
        links.retain(|_, (_, until)| *until > Instant::now());
        if links.len() >= 1000 {
            return Err("too many download links: try again in an hour".into());
        }
        links.insert(id.clone(), (file, Instant::now() + LINK_LIFE));
        Ok(id)
    }

    /// The file a download link names, while it works.
    pub fn link(&self, id: &str) -> Option<PathBuf> {
        let links = self.links.lock().unwrap_or_else(PoisonError::into_inner);
        links.get(id).filter(|(_, until)| *until > Instant::now()).map(|(f, _)| f.clone())
    }

    /// What the launcher adds to the app's answer to `initialize`: where the client's files are,
    /// how long they're kept and which expire soon, so the client can tell its user.
    pub fn instructions(&self, app: &str, client: &str) -> String {
        let (days, quota, from_url) = {
            let s = self.settings();
            (s.retention_days, s.quota_bytes, s.file_from_url)
        };
        let (files, more) = self.files(app, client);
        let used = self.client_dir(app, client).map_or(0, |d| dir_size(&d));
        let mut text = String::from(
            "ArtCraft Launcher server: this connection has a work folder of its own on the server (paths are relative to it; \
             other connections can't see it). ",
        );
        text.push_str(&format!("It holds {} files, {} MB of {} MB. ", files.len() + more as usize, used >> 20, quota >> 20));
        if days == 0 {
            text.push_str("Files are kept until the administrator deletes them. ");
        } else {
            text.push_str(&format!("Files not changed for {days} days are deleted automatically. "));
            let soon = artcraft_launcher_native::now() + EXPIRING_SOON;
            let expiring: Vec<String> = files
                .iter()
                .filter(|f| f.expires.is_some_and(|e| e <= soon))
                .take(20)
                .map(|f| format!("{} ({})", f.path, date(f.expires.unwrap_or(0))))
                .collect();
            if !expiring.is_empty() {
                text.push_str(&format!(
                    "Expiring within two days, unless changed: {}. Tell the user, so they can download what they want to keep. ",
                    expiring.join(", ")
                ));
            }
        }
        text.push_str(&format!(
            "Documents opened with the app's tools live in this session's memory: save them into the folder to keep them. \
             A session unused for {} minutes ends, and what wasn't saved is lost. ",
            IDLE.as_secs() / 60
        ));
        text.push_str(
            "Tools: launcher_files_list lists the files with the date each expires; launcher_file_link gives a link (valid one hour) \
             the user can open to download a file — give it to them when they want a result.",
        );
        if from_url {
            text.push_str(" launcher_file_from_url downloads a file from a web address into the folder, to work on it.");
        }
        text
    }

    /// The launcher's own tools, added to each app's.
    pub fn launcher_tools(&self) -> Vec<Value> {
        let mut tools = vec![
            serde_json::json!({
                "name": "launcher_files_list",
                "description": "Lists the files in this connection's work folder on the ArtCraft Launcher server: path, size, last change and when it will be deleted automatically.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
                "annotations": {"readOnlyHint": true}
            }),
            serde_json::json!({
                "name": "launcher_file_link",
                "description": "Gives a link to download a file of this connection's work folder, valid for one hour. Give it to the user so they can save the file.",
                "inputSchema": {"type": "object", "properties": {"path": {"type": "string", "description": "The file's path, relative to the work folder."}}, "required": ["path"], "additionalProperties": false},
                "annotations": {"readOnlyHint": true}
            }),
        ];
        if self.settings().file_from_url {
            tools.push(serde_json::json!({
                "name": "launcher_file_from_url",
                "description": format!("Downloads a file from a public https address into this connection's work folder (at most {} MB), to open it with the app's tools.", FILE_MAX >> 20),
                "inputSchema": {"type": "object", "properties": {
                    "url": {"type": "string", "description": "A public https address."},
                    "path": {"type": "string", "description": "Where to save it, relative to the work folder (default: the address's file name)."}
                }, "required": ["url"], "additionalProperties": false}
            }));
        }
        tools
    }

    /// Runs one of the launcher's tools for a client; the `tools/call` result.
    pub async fn call_launcher_tool(
        self: &Arc<Self>,
        http: &artcraft_launcher_native::http::Http,
        app: &'static str,
        client: &str,
        name: &str,
        args: &Value,
        base: &str,
    ) -> Value {
        let text = |t: String, error: bool| serde_json::json!({"content": [{"type": "text", "text": t}], "isError": error});
        let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
        match name {
            "launcher_files_list" => {
                let (files, more) = self.files(app, client);
                let quota = self.settings().quota_bytes;
                let used = self.client_dir(app, client).map_or(0, |d| dir_size(&d));
                let list: Vec<Value> = files
                    .iter()
                    .map(|f| serde_json::json!({"path": f.path, "bytes": f.bytes, "modified": date(f.modified), "expires": f.expires.map(date)}))
                    .collect();
                let answer = serde_json::json!({"files": list, "more": more, "used_bytes": used, "quota_bytes": quota});
                text(answer.to_string(), false)
            }
            "launcher_file_link" => {
                let path = arg("path");
                match self.file_path(app, client, &path).filter(|f| f.is_file()) {
                    Some(file) => match self.new_link(file) {
                        Ok(id) => text(format!("{base}/files/{id}/{}", url_name(&path)), false),
                        Err(e) => text(e, true),
                    },
                    None => text(format!("there's no file {path} in the work folder (launcher_files_list lists them)"), true),
                }
            }
            "launcher_file_from_url" if self.settings().file_from_url => {
                let url = arg("url");
                let path = match arg("path") {
                    p if p.is_empty() => url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string(),
                    p => p,
                };
                let Some(file) = self.file_path(app, client, &path) else {
                    return text("give a file name (path) to save it as".into(), true);
                };
                let Some(dir) = self.client_dir(app, client) else { return text("no work folder".into(), true) };
                let quota = self.settings().quota_bytes;
                let room = quota.saturating_sub(dir_size(&dir)).min(FILE_MAX);
                let (http, fetch_url) = (http.clone(), url.clone());
                let got = tokio::task::spawn_blocking(move || http.get_checked(&fetch_url, room, &public_address)).await;
                match got.map_err(|e| e.to_string()).and_then(|r| r) {
                    Ok(bytes) => {
                        let saved = file
                            .parent()
                            .map_or(Ok(()), std::fs::create_dir_all)
                            .map_err(|e| e.to_string())
                            .and_then(|()| artcraft_launcher_native::paths::write_atomic(&file, &bytes).map_err(|e| e.to_string()));
                        self.forget_sizes();
                        match saved {
                            Ok(()) => text(format!("saved {path} ({} bytes)", bytes.len()), false),
                            Err(e) => text(format!("couldn't save {path} ({e})"), true),
                        }
                    }
                    Err(e) => text(format!("couldn't download {url}: {e}"), true),
                }
            }
            _ => text(format!("no such tool: {name}"), true),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.settings().enabled = on;
        self.save();
        if !on {
            self.close_where(|_| true);
        }
    }

    pub fn set_retention(&self, days: u32) -> Result<(), String> {
        if !RETENTION_CHOICES.contains(&days) {
            return Err(format!("{days} days isn't one of the choices"));
        }
        self.settings().retention_days = days;
        self.save();
        Ok(())
    }

    pub fn set_quota(&self, bytes: u64) -> Result<(), String> {
        if !(64 << 20..=1 << 40).contains(&bytes) {
            return Err("the quota must be between 64 MB and 1 TB".into());
        }
        self.settings().quota_bytes = bytes;
        self.save();
        Ok(())
    }

    pub fn set_max_sessions(&self, sessions: u32) -> Result<(), String> {
        if !(1..=64).contains(&sessions) {
            return Err("sessions at once must be between 1 and 64".into());
        }
        self.settings().max_sessions = sessions;
        self.save();
        Ok(())
    }

    /// Turns an app's MCP server on or off (its tool is fetched by the caller).
    pub fn set_app(&self, app: &CraftApp, on: bool) -> Result<(), String> {
        if on {
            let dir = self.work_dir(app.id);
            std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
            self.settings().apps.insert(app.id.to_string());
        } else {
            self.settings().apps.remove(app.id);
            self.close_where(|s| s.app == app.id);
        }
        self.save();
        Ok(())
    }

    /// Deletes everything in an app's work folder (every client's), ending its sessions first.
    pub fn empty(&self, app: &CraftApp) -> Result<(), String> {
        self.close_where(|s| s.app == app.id);
        let dir = self.work_dir(app.id);
        empty_dir(&dir)?;
        self.forget_sizes();
        log::info!("{}: MCP work folder emptied", app.name);
        Ok(())
    }

    /// An app is being uninstalled: its MCP server goes off and its work folder goes.
    pub fn forget_app(&self, app: &CraftApp) {
        self.settings().apps.remove(app.id);
        self.save();
        self.close_where(|s| s.app == app.id);
        let dir = self.work_dir(app.id);
        if dir.exists()
            && let Err(e) = std::fs::remove_dir_all(&dir)
        {
            log::warn!("couldn't remove {} ({e})", dir.display());
        }
        self.forget_sizes();
    }

    /// Makes a token; returns its secret, which is never shown again.
    pub fn create_token(&self, name: &str, apps: Option<Vec<String>>) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err("give the token a name of up to 80 characters".into());
        }
        if let Some(list) = &apps
            && (list.is_empty() || list.iter().any(|a| CraftApp::by_id(a).is_none()))
        {
            return Err("unknown app".into());
        }
        let secret = format!("{TOKEN_PREFIX}{}", random_text(32)?);
        let hint: String = secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        let token = StoredToken {
            id: random_text(9)?,
            name: name.to_string(),
            hash: fingerprint(&secret),
            hint,
            apps,
            created_at: artcraft_launcher_native::now(),
            last_used: None,
            grant: None,
        };
        let mut settings = self.settings();
        if settings.tokens.len() >= 100 {
            return Err("there are 100 tokens already: revoke some first".into());
        }
        settings.tokens.push(token);
        drop(settings);
        self.save();
        Ok(secret)
    }

    /// Revokes a token: its sessions end, and its work folders go (the page asks first).
    pub fn revoke_token(&self, id: &str) {
        self.settings().tokens.retain(|t| t.id != id);
        self.save();
        self.close_where(|s| s.token == id);
        if id == PREVIOUS_FILES {
            return;
        }
        for app in &SUITE {
            if let Some(dir) = self.client_dir(app.id, id)
                && dir.exists()
                && let Err(e) = std::fs::remove_dir_all(&dir)
            {
                log::warn!("couldn't remove {} ({e})", dir.display());
            }
        }
        self.forget_sizes();
    }

    /// Who may use `app`: the id of the token the request signs in with.
    pub fn authorize(&self, authorization: Option<&str>, app: &str, hosting: &Hosting) -> Result<String, Refusal> {
        {
            let s = self.settings();
            if !s.enabled || !s.apps.contains(app) {
                return Err(Refusal::Off);
            }
        }
        if hosting.cli(app).is_none() {
            return Err(Refusal::Off);
        }
        let secret = authorization
            .and_then(|v| v.trim().split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, t)| t.trim())
            .filter(|t| t.starts_with(TOKEN_PREFIX) && t.len() < 200)
            .ok_or(Refusal::SignIn)?;
        let hash = fingerprint(secret);
        let mut s = self.settings();
        let token = s.tokens.iter_mut().find(|t| same(t.hash.as_bytes(), hash.as_bytes())).ok_or(Refusal::SignIn)?;
        // An OAuth client's expired access token: it refreshes it.
        if token.grant.as_ref().is_some_and(|g| g.access_expires <= artcraft_launcher_native::now()) {
            return Err(Refusal::SignIn);
        }
        if token.apps.as_ref().is_some_and(|list| !list.iter().any(|a| a == app)) {
            return Err(Refusal::Forbidden);
        }
        token.last_used = Some(artcraft_launcher_native::now());
        let id = token.id.clone();
        drop(s);
        self.dirty.store(true, Ordering::SeqCst);
        Ok(id)
    }

    /// The session `id`, if it belongs to this app and token.
    pub fn session(&self, id: &str, app: &str, token: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.get(id).filter(|s| s.app == app && s.token == token && !s.closed.load(Ordering::SeqCst)).cloned()
    }

    pub fn end_session(&self, id: &str) {
        if let Some(s) = self.sessions.lock().unwrap_or_else(PoisonError::into_inner).remove(id) {
            s.close();
        }
    }

    /// Whether a client's work folder is over the quota (checked before each tool call).
    pub fn over_quota(&self, app: &'static str, client: &str) -> Option<(u64, u64)> {
        let quota = self.settings().quota_bytes;
        let used = dir_size(&self.client_dir(app, client)?);
        (used > quota).then_some((used, quota))
    }

    /// Starts the app's tool for a new session.
    pub fn start(self: &Arc<Self>, app: &'static CraftApp, token: String, cli: &Path) -> Result<Arc<Session>, String> {
        let max = usize::try_from(self.settings().max_sessions).unwrap_or(usize::MAX);
        {
            // Clients (claude.ai among them) often start a new session without ending the last
            // one: when full, the session left alone longest makes room, unless every session is
            // waiting for an answer right now.
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            while sessions.len() >= max {
                let idle = sessions
                    .values()
                    .filter(|s| s.pending.lock().unwrap_or_else(PoisonError::into_inner).is_empty())
                    .max_by_key(|s| s.idle())
                    .map(|s| s.id.clone());
                let Some(id) = idle else {
                    log::warn!("{}: refused a new MCP session: {max} sessions are busy", app.name);
                    return Err(format!("{max} MCP sessions are busy already; try again in a moment"));
                };
                if let Some(old) = sessions.remove(&id) {
                    log::info!("{}: closed an idle MCP session of {} to make room", app.name, old.app);
                    old.close();
                }
            }
        }
        if !self.confinable(cli) {
            return Err(format!(
                "this release of {}'s command-line tool can't be confined to a work folder (it has no --automation-read-root), \
                 so the launcher doesn't run it; a newer release of {} will work",
                app.name, app.name
            ));
        }
        let dir = self.client_dir(app.id, &token).ok_or("no work folder for this token")?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
        let mut child = tokio::process::Command::new(cli)
            .arg("mcp")
            .arg("--automation-read-root")
            .arg(&dir)
            .arg("--automation-write-root")
            .arg(&dir)
            .current_dir(&dir)
            .env_clear()
            .env("HOME", &dir)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("couldn't start {}'s MCP server ({e})", app.name))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take();
        let session = Arc::new(Session {
            id: random_text(18)?,
            app: app.id,
            token,
            stdin: tokio::sync::Mutex::new(stdin),
            child: Mutex::new(Some(child)),
            pending: Mutex::default(),
            last_used: Mutex::new(Instant::now()),
            closed: AtomicBool::new(false),
        });
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner).insert(session.id.clone(), session.clone());
        log::info!("{}: MCP session started", app.name);

        let (this, reader) = (self.clone(), session.clone());
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.len() > MAX_LINE {
                    log::warn!("{}: dropped an MCP message of {} bytes", reader.app, line.len());
                    continue;
                }
                dispatch(&reader, line);
            }
            this.end_session(&reader.id);
            log::info!("{}: MCP session ended", reader.app);
        });
        if let Some(stderr) = stderr {
            let app = app.id;
            tokio::spawn(async move {
                // What the tool says about itself (and why it stopped): the first lines of each
                // session in the log, the rest only when debugging.
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                let mut n = 0;
                while let Ok(Some(line)) = lines.next_line().await {
                    n += 1;
                    if n <= 20 {
                        log::info!("{app} mcp: {}", line.chars().take(500).collect::<String>());
                    } else {
                        log::debug!("{app} mcp: {line}");
                    }
                }
            });
        }
        Ok(session)
    }

    /// Whether the MCP server is on (the consent page says so when it isn't).
    pub fn enabled(&self) -> bool {
        self.settings().enabled
    }

    /// Registers an OAuth client (RFC 7591); a confidential one also gets a secret.
    pub fn register_client(&self, name: &str, redirect_uris: Vec<String>, confidential: bool) -> Result<(OAuthClient, Option<String>), String> {
        if redirect_uris.is_empty() || redirect_uris.len() > 10 || !redirect_uris.iter().all(|u| redirect_allowed(u)) {
            return Err("redirect_uris must be https addresses (or http on this machine)".into());
        }
        let name: String = name.trim().chars().take(80).collect();
        let secret = if confidential { Some(format!("acls_{}", random_text(32)?)) } else { None };
        let client = OAuthClient {
            client_id: format!("aclc_{}", random_text(12)?),
            client_name: if name.is_empty() { "MCP client".into() } else { name },
            redirect_uris,
            secret_hash: secret.as_deref().map(fingerprint),
            created_at: artcraft_launcher_native::now(),
        };
        let mut s = self.settings();
        // Registration is open (approving still takes the administrator's password): keep the
        // newest 200, dropping those no token was granted to first.
        if s.clients.len() >= 200 {
            let used: BTreeSet<String> = s.tokens.iter().filter_map(|t| t.grant.as_ref().map(|g| g.client_id.clone())).collect();
            if let Some(i) = s.clients.iter().position(|c| !used.contains(&c.client_id)) {
                s.clients.remove(i);
            } else {
                s.clients.remove(0);
            }
        }
        s.clients.push(client.clone());
        drop(s);
        self.save();
        Ok((client, secret))
    }

    /// A registered client, or one whose metadata document was read lately.
    pub fn client(&self, client_id: &str) -> Option<ClientInfo> {
        if let Some(c) = self.settings().clients.iter().find(|c| c.client_id == client_id) {
            return Some(ClientInfo {
                client_id: c.client_id.clone(),
                name: c.client_name.clone(),
                redirect_uris: c.redirect_uris.clone(),
                secret_hash: c.secret_hash.clone(),
            });
        }
        let docs = self.documents.lock().unwrap_or_else(PoisonError::into_inner);
        docs.get(client_id).filter(|(_, at)| at.elapsed() < Duration::from_secs(3600)).map(|(c, _)| c.clone())
    }

    /// Keeps a client described by its metadata document for an hour.
    pub fn remember_document(&self, client_id: &str, name: &str, redirect_uris: Vec<String>) -> ClientInfo {
        let info = ClientInfo {
            client_id: client_id.to_string(),
            name: name.chars().take(80).collect(),
            redirect_uris: redirect_uris.into_iter().filter(|u| redirect_allowed(u)).collect(),
            secret_hash: None,
        };
        self.documents.lock().unwrap_or_else(PoisonError::into_inner).insert(client_id.to_string(), (info.clone(), Instant::now()));
        info
    }

    /// Whether the client authenticated as it registered: a confidential one with its secret.
    pub fn client_authenticates(&self, client: &ClientInfo, secret: Option<&str>) -> bool {
        match (&client.secret_hash, secret) {
            (None, _) => true,
            (Some(hash), Some(secret)) => same(hash.as_bytes(), fingerprint(secret).as_bytes()),
            (Some(_), None) => false,
        }
    }

    /// The administrator approved: a one-time code for the client to redeem.
    pub fn new_code(&self, client: &ClientInfo, redirect_uri: &str, challenge: &str, apps: Option<Vec<String>>) -> Result<String, String> {
        let code = random_text(32)?;
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        codes.retain(|_, c| c.expires > Instant::now());
        codes.insert(
            code.clone(),
            AuthCode {
                client_id: client.client_id.clone(),
                client_name: client.name.clone(),
                redirect_uri: redirect_uri.to_string(),
                challenge: challenge.to_string(),
                apps,
                expires: Instant::now() + CODE_LIFE,
            },
        );
        Ok(code)
    }

    /// Redeems a code (once) for tokens, checking the PKCE verifier.
    pub fn redeem_code(&self, code: &str, client_id: &str, redirect_uri: &str, verifier: &str) -> Result<TokenSet, OAuthError> {
        let found = self.codes.lock().unwrap_or_else(PoisonError::into_inner).remove(code).ok_or("invalid_grant")?;
        if found.expires <= Instant::now() || found.client_id != client_id || found.redirect_uri != redirect_uri {
            return Err("invalid_grant");
        }
        if !(43..=128).contains(&verifier.len()) || !same(pkce_challenge(verifier).as_bytes(), found.challenge.as_bytes()) {
            return Err("invalid_grant");
        }
        let (access, refresh) = (format!("{TOKEN_PREFIX}{}", random_text(32).map_err(|_| "server_error")?), self.new_refresh()?);
        let now = artcraft_launcher_native::now();
        let token = StoredToken {
            id: random_text(9).map_err(|_| "server_error")?,
            name: found.client_name,
            hash: fingerprint(&access),
            hint: access.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect(),
            apps: found.apps,
            created_at: now,
            last_used: None,
            grant: Some(Grant {
                client_id: found.client_id,
                access_expires: now + ACCESS_LIFE,
                refresh_hash: fingerprint(&refresh),
                refresh_expires: now + REFRESH_LIFE,
            }),
        };
        let mut s = self.settings();
        if s.tokens.len() >= 100 {
            return Err("access_denied");
        }
        s.tokens.push(token);
        drop(s);
        self.save();
        Ok(TokenSet { access_token: access, token_type: "Bearer", expires_in: ACCESS_LIFE, refresh_token: refresh })
    }

    /// A new access token for a refresh token; the refresh token is replaced too.
    pub fn refresh(&self, refresh: &str, client_id: &str) -> Result<TokenSet, OAuthError> {
        let hash = fingerprint(refresh);
        let now = artcraft_launcher_native::now();
        let (access, new_refresh) = (format!("{TOKEN_PREFIX}{}", random_text(32).map_err(|_| "server_error")?), self.new_refresh()?);
        let mut s = self.settings();
        let token = s
            .tokens
            .iter_mut()
            .find(|t| t.grant.as_ref().is_some_and(|g| same(g.refresh_hash.as_bytes(), hash.as_bytes())))
            .ok_or("invalid_grant")?;
        let grant = token.grant.as_mut().ok_or("invalid_grant")?;
        if grant.client_id != client_id || grant.refresh_expires <= now {
            return Err("invalid_grant");
        }
        grant.access_expires = now + ACCESS_LIFE;
        grant.refresh_hash = fingerprint(&new_refresh);
        grant.refresh_expires = now + REFRESH_LIFE;
        token.hash = fingerprint(&access);
        token.hint = access.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        drop(s);
        self.save();
        Ok(TokenSet { access_token: access, token_type: "Bearer", expires_in: ACCESS_LIFE, refresh_token: new_refresh })
    }

    fn new_refresh(&self) -> Result<String, OAuthError> {
        random_text(32).map(|r| format!("{REFRESH_PREFIX}{r}")).map_err(|_| "server_error")
    }

    fn close_where(&self, mut which: impl FnMut(&Session) -> bool) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<String> = sessions.values().filter(|s| which(s)).map(|s| s.id.clone()).collect();
        for id in ids {
            if let Some(s) = sessions.remove(&id) {
                s.close();
            }
        }
    }

    /// Runs every minute: ends idle sessions, saves when tokens were used, and once a day deletes
    /// files not changed for `retention_days`.
    pub fn maintain(&self) {
        self.close_where(|s| s.idle() > IDLE);
        let (days, last) = {
            let s = self.settings();
            (s.retention_days, s.last_cleanup)
        };
        let now = artcraft_launcher_native::now();
        if days > 0 && now - last >= 24 * 3600 {
            let cutoff = std::time::SystemTime::now() - Duration::from_secs(u64::from(days) * 24 * 3600);
            for app in &SUITE {
                let removed = remove_older(&self.work_dir(app.id), cutoff, true);
                if removed > 0 {
                    log::info!("{}: removed {removed} MCP work files older than {days} days", app.name);
                    self.forget_sizes();
                }
            }
            self.settings().last_cleanup = now;
            self.save();
        } else if self.dirty.load(Ordering::SeqCst) {
            self.save();
        }
    }
}

/// Routes one line from a tool: an answer to the request waiting for it (which ends that wait),
/// anything else to every request waiting on this session.
fn dispatch(session: &Session, line: String) {
    let Ok(value) = serde_json::from_str::<Value>(&line) else {
        log::debug!("{}: not JSON from the MCP server: {line}", session.app);
        return;
    };
    let answer = value.get("method").is_none() && (value.get("result").is_some() || value.get("error").is_some());
    let mut pending = session.pending.lock().unwrap_or_else(PoisonError::into_inner);
    if answer && let Some(id) = value.get("id") {
        if let Some(tx) = pending.remove(&id.to_string()) {
            let _ = tx.send(line);
        }
        return;
    }
    for tx in pending.values() {
        let _ = tx.send(line.clone());
    }
}

/// Waits for the answer among what `rx` brings, ignoring the rest.
pub async fn answer(mut rx: mpsc::UnboundedReceiver<String>, id: &Value) -> Option<String> {
    let wait = async {
        while let Some(line) = rx.recv().await {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v.get("id") == Some(id)
                && v.get("method").is_none()
            {
                return Some(line);
            }
        }
        None
    };
    tokio::time::timeout(ANSWER_WAIT, wait).await.ok().flatten()
}

/// A JSON-RPC error answer.
pub fn rpc_error(id: Option<&Value>, code: i64, message: &str) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id.cloned().unwrap_or(Value::Null), "error": {"code": code, "message": message}}).to_string()
}

/// Where an OAuth client may be sent back to: https, or http on this machine (desktop clients).
pub fn redirect_allowed(uri: &str) -> bool {
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|p| uri.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/'])));
    (uri.starts_with("https://") || local) && uri.len() <= 512 && !uri.contains('#') && !uri.chars().any(char::is_whitespace)
}

/// PKCE S256: base64url(SHA-256(verifier)) without padding.
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn fingerprint(secret: &str) -> String {
    artcraft_launcher_native::http::hex(&Sha256::digest(secret.as_bytes()))
}

fn same(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// `n` random bytes as URL-safe text.
fn random_text(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness available ({e})"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Starts `cli mcp` confined to `root`, as sessions start it, and asks it to initialize: `Ok`
/// when it answers, else why not (what it wrote on stderr). Releases from before
/// `--automation-read-root` reject the option and exit; their `--help` can't tell, since some
/// apps that take it don't answer `--help`.
fn answers_confined(cli: &Path, root: &Path) -> Result<(), String> {
    use std::io::{BufRead, Write};
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(cli)
        .arg("mcp")
        .arg("--automation-read-root")
        .arg(root)
        .arg("--automation-write-root")
        .arg(root)
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("it doesn't start ({e})"))?;
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"artcraft-launcher","version":"1"}}}"#;
    // Kept open until the answer: some tools stop at the end of their input.
    let mut stdin = child.stdin.take();
    if let Some(input) = stdin.as_mut() {
        let _ = writeln!(input, "{init}");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("\"result\"") {
                    let _ = tx.send(());
                    return;
                }
            }
        });
    }
    let answered = rx.recv_timeout(Duration::from_secs(10)).is_ok();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let why = child
        .stderr
        .take()
        .and_then(|e| std::io::BufReader::new(e).lines().map_while(Result::ok).find(|l| !l.contains("files confined") && !l.trim().is_empty()))
        .unwrap_or_else(|| "it didn't answer".into());
    let _ = std::fs::remove_dir_all(root);
    if answered { Ok(()) } else { Err(why.chars().take(300).collect()) }
}

/// Adds the files under `dir` to `out`, with paths starting with `prefix` (links aren't
/// followed, and no deeper than 16 folders).
fn list_files(dir: &Path, prefix: &str, out: &mut Vec<McpFile>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else { continue };
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        match e.file_type() {
            Ok(t) if t.is_dir() && depth < 16 => list_files(&e.path(), &path, out, depth + 1),
            Ok(t) if t.is_file() => {
                let meta = e.metadata().ok();
                let modified = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
                out.push(McpFile { path, bytes: meta.map_or(0, |m| m.len()), modified, expires: None });
            }
            _ => {}
        }
    }
}

/// Unix seconds as an RFC 3339 date and time, in UTC.
fn date(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rest = secs.rem_euclid(86400);
    // Howard Hinnant's days-from-civil, backwards.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// The last part of a path, for the end of a download link (so it's saved under that name).
fn url_name(path: &str) -> String {
    artcraft_launcher_core::hosted::url_component(path.rsplit('/').next().unwrap_or(path))
}

/// Whether `url` may be fetched for a client: https, to a host whose every address is public (not
/// this machine, the local network, or the cloud's metadata service).
fn public_address(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("https://").ok_or("only https addresses are fetched")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err("addresses with a user name aren't fetched".into());
    }
    // `host`, `host:port`, `[v6]` or `[v6]:port`.
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']').ok_or("a bad address")?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = port.map_or(Ok(443), str::parse::<u16>).map_err(|_| "a bad port")?;
    if host.is_empty() {
        return Err("an address without a host".into());
    }
    use std::net::ToSocketAddrs;
    let addresses: Vec<std::net::SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("couldn't find {host} ({e})"))?.collect();
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        return Err(format!("{host} isn't a public address"));
    }
    Ok(())
}

fn public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
                || a >= 224)
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_ip(std::net::IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Bytes of the files under `dir` (links aren't followed).
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .fold(0u64, u64::saturating_add)
}

/// Deletes what's in `dir`, keeping `dir`.
fn empty_dir(dir: &Path) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in entries.flatten() {
        let path = e.path();
        let result = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        result.map_err(|err| format!("couldn't remove {} ({err})", path.display()))?;
    }
    Ok(())
}

/// Deletes files under `dir` last changed before `cutoff`, then folders left empty (not `dir`
/// itself, `top`); returns how many files went.
fn remove_older(dir: &Path, cutoff: std::time::SystemTime, top: bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten() {
        let path = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => removed += remove_older(&path, cutoff, false),
            Ok(_) => {
                let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff);
                if old && std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
            Err(_) => {}
        }
    }
    if !top && std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none()) {
        let _ = std::fs::remove_dir(dir);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp() -> (tempfile::TempDir, Mcp) {
        let dir = tempfile::tempdir().unwrap();
        let mcp = Mcp::open(dir.path(), dir.path().join("work")).unwrap();
        (dir, mcp)
    }

    #[test]
    fn tokens_are_shown_once_and_kept_as_fingerprints() {
        let (dir, mcp) = mcp();
        let secret = mcp.create_token("Claude Desktop", None).unwrap();
        assert!(secret.starts_with("acl_") && secret.len() > 40);
        let saved = std::fs::read_to_string(dir.path().join("mcp.json")).unwrap();
        assert!(!saved.contains(&secret), "the secret itself is never stored");
        assert!(saved.contains(&fingerprint(&secret)));
        assert!(mcp.create_token("  ", None).is_err());
        assert!(mcp.create_token("x", Some(vec!["nope".into()])).is_err());
    }

    #[test]
    fn an_oauth_client_gets_tokens_once_and_refreshes_them() {
        let (_dir, mcp) = mcp();
        assert!(mcp.register_client("x", vec!["ftp://nope".into()], false).is_err());
        let (client, secret) = mcp.register_client("Claude", vec!["https://claude.ai/api/mcp/auth_callback".into()], false).unwrap();
        assert!(secret.is_none());
        let info = mcp.client(&client.client_id).unwrap();
        let verifier = "a".repeat(50);
        let code = mcp.new_code(&info, "https://claude.ai/api/mcp/auth_callback", &pkce_challenge(&verifier), None).unwrap();
        // A wrong verifier spends nothing; the code works once.
        assert_eq!(
            mcp.redeem_code(&code, &client.client_id, "https://claude.ai/api/mcp/auth_callback", &"b".repeat(50)).unwrap_err(),
            "invalid_grant"
        );
        let code = mcp.new_code(&info, "https://claude.ai/api/mcp/auth_callback", &pkce_challenge(&verifier), None).unwrap();
        let tokens = mcp.redeem_code(&code, &client.client_id, "https://claude.ai/api/mcp/auth_callback", &verifier).unwrap();
        assert!(mcp.redeem_code(&code, &client.client_id, "https://claude.ai/api/mcp/auth_callback", &verifier).is_err());
        assert!(tokens.access_token.starts_with("acl_") && tokens.refresh_token.starts_with("aclr_"));
        let renewed = mcp.refresh(&tokens.refresh_token, &client.client_id).unwrap();
        assert_ne!(renewed.access_token, tokens.access_token);
        assert!(mcp.refresh(&tokens.refresh_token, &client.client_id).is_err(), "a refresh token works once");
        assert!(mcp.refresh(&renewed.refresh_token, "someone-else").is_err());
        let state_tokens = mcp.settings().tokens.clone();
        assert_eq!(state_tokens.len(), 1);
        assert!(state_tokens[0].grant.is_some());
    }

    #[test]
    fn redirects_go_only_to_https_or_this_machine() {
        assert!(redirect_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_allowed("http://localhost:33418/callback"));
        assert!(redirect_allowed("http://127.0.0.1/cb"));
        assert!(!redirect_allowed("http://localhost.evil.com/cb"));
        assert!(!redirect_allowed("http://example.com/cb"));
        assert!(!redirect_allowed("javascript:alert(1)"));
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn retention_and_limits_only_take_sensible_values() {
        let (_dir, mcp) = mcp();
        assert!(mcp.set_retention(7).is_ok() && mcp.set_retention(5).is_err());
        assert!(mcp.set_quota(2 << 30).is_ok() && mcp.set_quota(1).is_err());
        assert!(mcp.set_max_sessions(4).is_ok() && mcp.set_max_sessions(0).is_err());
    }

    #[test]
    fn old_files_go_and_new_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/old.png"), b"x").unwrap();
        std::fs::write(dir.path().join("new.png"), b"xy").unwrap();
        assert_eq!(dir_size(dir.path()), 3);
        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        // Everything is older than an hour from now: all files go, and the emptied folders.
        assert_eq!(remove_older(dir.path(), future, true), 2);
        assert!(dir.path().exists() && !dir.path().join("a").exists());
        std::fs::write(dir.path().join("kept.png"), b"x").unwrap();
        let past = std::time::SystemTime::now() - Duration::from_secs(3600);
        assert_eq!(remove_older(dir.path(), past, true), 0);
        empty_dir(dir.path()).unwrap();
        assert_eq!(dir_size(dir.path()), 0);
    }

    #[test]
    fn each_client_has_its_own_folder_and_the_old_files_go_to_previous() {
        let dir = tempfile::tempdir().unwrap();
        // Files from when clients shared the app's folder, and settings from before `layout`.
        std::fs::create_dir_all(dir.path().join("work/photocraft/out")).unwrap();
        std::fs::write(dir.path().join("work/photocraft/cover.png"), b"png").unwrap();
        std::fs::write(dir.path().join("work/photocraft/out/a.pcraft"), b"pc").unwrap();
        std::fs::write(dir.path().join("mcp.json"), br#"{"enabled": true, "max_sessions": 4}"#).unwrap();
        let mcp = Mcp::open(dir.path(), dir.path().join("work")).unwrap();
        assert_eq!(mcp.settings().max_sessions, 10, "the old default is raised");
        assert!(dir.path().join("work/photocraft/previous/cover.png").is_file());
        assert!(dir.path().join("work/photocraft/previous/out/a.pcraft").is_file());
        assert!(!dir.path().join("work/photocraft/cover.png").exists());

        mcp.create_token("Claude", None).unwrap();
        let id = mcp.settings().tokens[0].id.clone();
        let app = CraftApp::by_id("photocraft").unwrap();
        mcp.save_file(app, &id, "in/edinburgh.jpg", b"jpeg").unwrap();
        assert!(mcp.save_file(app, PREVIOUS_FILES, "x.png", b"x").is_err(), "nobody adds to the previous files");
        assert!(mcp.save_file(app, "nobody", "x.png", b"x").is_err());
        let (files, more) = mcp.files("photocraft", &id);
        assert_eq!((files.len(), more), (1, 0));
        assert_eq!(files[0].path, "in/edinburgh.jpg");
        assert_eq!(files[0].expires, Some(files[0].modified + 7 * 24 * 3600));

        let state =
            mcp.state(&crate::hosting::Hosting::open(dir.path(), artcraft_launcher_native::http::Http::new().unwrap()).unwrap(), "mcp/", true);
        let groups: Vec<(&str, &str)> = state.files.iter().map(|g| (g.client.as_str(), g.name.as_str())).collect();
        assert!(groups.contains(&(id.as_str(), "Claude")) && groups.contains(&(PREVIOUS_FILES, "")));
        assert!(mcp.instructions("photocraft", &id).contains("1 files"));

        // Revoking a token deletes its folders.
        mcp.revoke_token(&id);
        assert!(!dir.path().join("work/photocraft").join(&id).exists());
        assert!(dir.path().join("work/photocraft/previous/cover.png").is_file());
        mcp.delete_file(app, PREVIOUS_FILES, "cover.png").unwrap();
        assert!(!dir.path().join("work/photocraft/previous/cover.png").exists());
    }

    #[test]
    fn paths_stay_inside_the_clients_folder() {
        let (dir, mcp) = mcp();
        assert!(mcp.file_path("photocraft", "abc", "a/b.png").is_some());
        for bad in ["", "../x", "a/../../x", "/etc/passwd", "a\\..\\x"] {
            assert!(mcp.file_path("photocraft", "abc", bad).is_none(), "{bad}");
        }
        assert!(mcp.client_dir("photocraft", "../x").is_none());
        assert!(mcp.client_dir("photocraft", "").is_none());
        #[cfg(unix)]
        {
            std::fs::create_dir_all(dir.path().join("work/photocraft/abc")).unwrap();
            std::os::unix::fs::symlink("/etc", dir.path().join("work/photocraft/abc/etc")).unwrap();
            assert!(mcp.file_path("photocraft", "abc", "etc/passwd").is_none());
        }
    }

    #[test]
    fn dates_and_addresses() {
        assert_eq!(date(0), "1970-01-01T00:00:00Z");
        assert_eq!(date(1_792_108_800), "2026-10-16T00:00:00Z");
        assert_eq!(date(951_782_400 + 3661), "2000-02-29T01:01:01Z");
        for private in
            ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"]
        {
            assert!(!public_ip(private.parse().unwrap()), "{private}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()) && public_ip("2606:4700::1111".parse().unwrap()));
        assert!(public_address("http://example.com/a").is_err());
        assert!(public_address("https://127.0.0.1/a").is_err());
        assert!(public_address("https://[::1]:8443/a").is_err());
        assert!(public_address("https://user@1.1.1.1/a").is_err());
        assert!(public_address("https://1.1.1.1:443/a.png").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_full_server_closes_its_idlest_session_for_a_new_one() {
        use std::os::unix::fs::PermissionsExt;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (dir, mcp) = mcp();
            let mcp = Arc::new(mcp);
            // A stand-in tool that just stays up.
            let cli = dir.path().join("fake-cli");
            std::fs::write(&cli, "#!/bin/sh\nread line\necho '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nexec sleep 30\n").unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            mcp.set_max_sessions(2).unwrap();
            assert_eq!(Mcp::open(dir.path(), dir.path().join("work")).unwrap().settings().max_sessions, 2, "a chosen number stays");
            let app = CraftApp::by_id("deckcraft").unwrap();
            let first = mcp.start(app, "tok1".into(), &cli).unwrap();
            let second = mcp.start(app, "tok1".into(), &cli).unwrap();
            *first.last_used.lock().unwrap() = Instant::now() - Duration::from_secs(60);
            let third = mcp.start(app, "tok2".into(), &cli).unwrap();
            let open: Vec<String> = mcp.sessions.lock().unwrap().keys().cloned().collect();
            assert_eq!(open.len(), 2);
            assert!(!open.contains(&first.id) && open.contains(&second.id) && open.contains(&third.id));
            assert!(first.closed.load(Ordering::SeqCst));
            mcp.close_where(|_| true);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_that_cant_be_confined_is_never_run() {
        use std::os::unix::fs::PermissionsExt;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (dir, mcp) = mcp();
            let mcp = Arc::new(mcp);
            // Like DeckCraft 0.7.0: it rejects --automation-read-root and exits.
            let cli = dir.path().join("old-cli");
            std::fs::write(&cli, "#!/bin/sh\necho 'unknown mcp option `--automation-read-root`' >&2\nexit 1\n").unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!mcp.confinable(&cli));
            let refused = mcp.start(CraftApp::by_id("deckcraft").unwrap(), "tok".into(), &cli).err().unwrap();
            assert!(refused.contains("can't be confined"), "{refused}");
            assert!(mcp.sessions.lock().unwrap().is_empty());
        });
    }

    /// `ARTCRAFT_PROBE_CLIS=a:b:c cargo test probe_real -- --ignored --nocapture`: tries the
    /// confinement check on real command-line tools.
    #[test]
    #[ignore]
    fn probe_real_tools() {
        let dir = tempfile::tempdir().unwrap();
        for cli in std::env::var("ARTCRAFT_PROBE_CLIS").unwrap_or_default().split(':').filter(|c| !c.is_empty()) {
            println!("{cli}: {:?}", answers_confined(Path::new(cli), &dir.path().join("root")));
        }
    }

    #[test]
    fn answers_go_to_their_request_and_the_rest_to_everyone() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // A stand-in tool: `cat` echoes requests back, which look like neither answers nor
            // notifications, so check dispatch directly with a session around a real process.
            let mut child = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "cat" })
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let session = Session {
                id: "s".into(),
                app: "photocraft",
                token: "t".into(),
                stdin: tokio::sync::Mutex::new(stdin),
                child: Mutex::new(Some(child)),
                pending: Mutex::default(),
                last_used: Mutex::new(Instant::now()),
                closed: AtomicBool::new(false),
            };
            let rx = session.send(&serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"})).await.unwrap().unwrap();
            dispatch(&session, r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#.into());
            dispatch(&session, r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#.into());
            let got = answer(rx, &serde_json::json!(7)).await.unwrap();
            assert!(got.contains("\"tools\""));
            assert!(session.pending.lock().unwrap().is_empty());
            session.close();
        });
    }
}
