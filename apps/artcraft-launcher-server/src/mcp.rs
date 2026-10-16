//! The Craft Apps as MCP servers, over MCP's Streamable HTTP transport at `/mcp/<app>`.
//!
//! Each app's command-line tool (`<app>-cli mcp`) is an MCP server over stdio. A client's
//! `initialize` starts one for its session, with the app's work folder as its only read and write
//! root; later requests carry the `Mcp-Session-Id` the answer gave. Requests are written to the
//! tool's stdin, and its answers are matched back by JSON-RPC id; what it sends meanwhile
//! (progress, logs) reaches the client too when it accepts an event stream.
//!
//! Only the administrator turns the MCP server and each app's on, and makes the access tokens
//! clients sign in with (stored as SHA-256 fingerprints, shown once). Work folders are emptied of
//! files not changed for `retention_days`, and a tool call is refused while its folder holds more
//! than the quota.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::catalog::{CraftApp, SUITE};
use artcraft_launcher_core::hosted::{McpApp, McpState, McpToken};
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
}

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
    /// Unix seconds of the last clean-up of old files.
    #[serde(default)]
    last_cleanup: i64,
}

fn default_retention() -> u32 {
    7
}
fn default_quota() -> u64 {
    2 << 30
}
fn default_sessions() -> u32 {
    4
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
            last_cleanup: 0,
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
    sizes: Mutex<HashMap<&'static str, (u64, Instant)>>,
    dirty: AtomicBool,
}

impl Mcp {
    pub fn open(data: &Path, work_root: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&work_root).map_err(|e| format!("couldn't create {} ({e})", work_root.display()))?;
        let settings_path = data.join("mcp.json");
        let settings = std::fs::read(&settings_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Ok(Self {
            settings_path,
            work_root,
            settings: Mutex::new(settings),
            sessions: Mutex::default(),
            sizes: Mutex::default(),
            dirty: AtomicBool::new(false),
        })
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

    /// Whether the app's MCP server is on (so updates fetch its tool too).
    pub fn app_on(&self, app: &str) -> bool {
        self.settings().apps.contains(app)
    }

    /// The administrator's view; `base_url` is where `/mcp/` is from the page asking.
    pub fn state(&self, hosting: &Hosting, base_url: &str) -> McpState {
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
                    used_bytes: self.used(a.id),
                    sessions: u32::try_from(sessions.iter().filter(|x| **x == a.id).count()).unwrap_or(u32::MAX),
                })
                .collect(),
            tokens: s
                .tokens
                .iter()
                .map(|t| McpToken {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    hint: t.hint.clone(),
                    apps: t.apps.clone(),
                    created_at: t.created_at,
                    last_used: t.last_used,
                })
                .collect(),
        }
    }

    /// Bytes in an app's work folder, measured at most every half minute.
    fn used(&self, app: &'static str) -> u64 {
        if let Some((bytes, at)) = self.sizes.lock().unwrap_or_else(PoisonError::into_inner).get(app)
            && at.elapsed() < SIZE_FRESH
        {
            return *bytes;
        }
        let bytes = dir_size(&self.work_dir(app));
        self.sizes.lock().unwrap_or_else(PoisonError::into_inner).insert(app, (bytes, Instant::now()));
        bytes
    }

    fn forget_size(&self, app: &str) {
        self.sizes.lock().unwrap_or_else(PoisonError::into_inner).remove(app);
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

    /// Deletes everything in an app's work folder, ending its sessions first.
    pub fn empty(&self, app: &CraftApp) -> Result<(), String> {
        self.close_where(|s| s.app == app.id);
        let dir = self.work_dir(app.id);
        empty_dir(&dir)?;
        self.forget_size(app.id);
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
        self.forget_size(app.id);
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

    pub fn revoke_token(&self, id: &str) {
        self.settings().tokens.retain(|t| t.id != id);
        self.save();
        self.close_where(|s| s.token == id);
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

    /// Whether the app's work folder is over its quota (checked before each tool call).
    pub fn over_quota(&self, app: &'static str) -> Option<(u64, u64)> {
        let quota = self.settings().quota_bytes;
        self.forget_size(app);
        let used = self.used(app);
        (used > quota).then_some((used, quota))
    }

    /// Starts the app's tool for a new session.
    pub fn start(self: &Arc<Self>, app: &'static CraftApp, token: String, cli: &Path) -> Result<Arc<Session>, String> {
        let max = usize::try_from(self.settings().max_sessions).unwrap_or(usize::MAX);
        if self.sessions.lock().unwrap_or_else(PoisonError::into_inner).len() >= max {
            return Err(format!("{max} MCP sessions are running already; try again later"));
        }
        let dir = self.work_dir(app.id);
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
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log::debug!("{app} mcp: {line}");
                }
            });
        }
        Ok(session)
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
                    self.forget_size(app.id);
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
