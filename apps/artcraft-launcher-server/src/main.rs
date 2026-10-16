//! ArtCraft Launcher server: the launcher's web page plus the Craft Apps' browser builds, on one
//! address, for running in Docker behind a domain.
//!
//! - `/` is the launcher (the `artcraft-launcher-web` build). Visitors see every Craft App and open
//!   the installed ones, which run in their own browser.
//! - `/apps/<app>/` is an installed app: its `<app>-web-<version>.zip` from the app's GitHub
//!   release, checked against the published SHA-256 and unpacked under the data folder.
//! - `/admin/login` signs the administrator in (`ARTCRAFT_ADMIN_PASSWORD`); the same page then
//!   installs, updates and removes apps and runs the MCP server.
//!
//! - `/mcp/<app>` is an app's MCP server, for MCP clients with an access token, when the
//!   administrator turned it on (`mcp.rs`).
//!
//! The API both pages use is described in `artcraft_launcher_core::hosted`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod auth;
mod hosting;
mod mcp;
mod oauth;
mod releases;
mod routes;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Everything a request handler can reach.
pub struct Server {
    pub hosting: Arc<hosting::Hosting>,
    pub mcp: Arc<mcp::Mcp>,
    /// For reading OAuth clients' metadata documents.
    pub http: artcraft_launcher_native::http::Http,
    pub releases: releases::Releases,
    /// The launcher's web build (`index.html`, its `.js` and `.wasm`).
    pub web_dir: PathBuf,
    /// `None`: no password is set, so nobody can install or remove apps.
    pub admin: Option<auth::Admin>,
    /// Signed-in administrators.
    pub sessions: auth::Sessions,
}

struct Config {
    listen: SocketAddr,
    data: PathBuf,
    web: PathBuf,
    work: PathBuf,
    admin: Option<auth::Admin>,
    check_every: Duration,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let listen = env("ARTCRAFT_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into());
        let listen = listen.parse().map_err(|e| format!("ARTCRAFT_LISTEN={listen} isn't an address like 0.0.0.0:8080 ({e})"))?;
        let password = match env("ARTCRAFT_ADMIN_PASSWORD_FILE") {
            Some(file) => Some(
                std::fs::read_to_string(&file)
                    .map_err(|e| format!("couldn't read ARTCRAFT_ADMIN_PASSWORD_FILE {file} ({e})"))?
                    .trim_end_matches(['\r', '\n'])
                    .to_string(),
            ),
            None => std::env::var("ARTCRAFT_ADMIN_PASSWORD").ok(),
        };
        let user = env("ARTCRAFT_ADMIN_USER").unwrap_or_else(|| "admin".into());
        let admin = match password.filter(|p| !p.is_empty()) {
            Some(p) => Some(auth::Admin::new(user, p)?),
            None => None,
        };
        let minutes = match env("ARTCRAFT_CHECK_MINUTES") {
            Some(m) => m.parse::<u64>().map_err(|_| format!("ARTCRAFT_CHECK_MINUTES={m} isn't a number of minutes"))?.max(1),
            None => 30,
        };
        Ok(Self {
            listen,
            data: env("ARTCRAFT_DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from),
            web: env("ARTCRAFT_WEB_DIR").map_or_else(|| PathBuf::from("/srv/artcraft-launcher"), PathBuf::from),
            work: env("ARTCRAFT_WORK_DIR").map_or_else(|| PathBuf::from("/work"), PathBuf::from),
            admin,
            check_every: Duration::from_secs(minutes * 60),
        })
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        std::process::exit(if healthcheck() { 0 } else { 1 });
    }
    Logger::install();
    if let Err(e) = run() {
        log::error!("{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    if !config.web.join("index.html").is_file() {
        return Err(format!("no launcher page in {}: set ARTCRAFT_WEB_DIR to the trunk build (dist/web)", config.web.display()));
    }
    let http = artcraft_launcher_native::http::Http::new()?;
    let hosting = Arc::new(hosting::Hosting::open(&config.data, http.clone())?);
    let releases = releases::Releases::new(http.clone(), &config.data, config.check_every);
    let mcp = Arc::new(mcp::Mcp::open(&config.data, config.work.clone())?);
    match &config.admin {
        Some(a) => log::info!("administrator sign-in at /admin/login (user {})", a.user()),
        None => log::warn!("ARTCRAFT_ADMIN_PASSWORD isn't set: apps can be opened, but nobody can install, update or remove them"),
    }
    let sessions = auth::Sessions::open(&config.data);
    let server = Arc::new(Server { hosting, mcp, http, releases, web_dir: config.web, admin: config.admin, sessions });
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| format!("couldn't start ({e})"))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(config.listen).await.map_err(|e| format!("couldn't listen on {} ({e})", config.listen))?;
        log::info!("ArtCraft Launcher server listening on http://{}", config.listen);
        let upkeep = server.mcp.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(60));
            loop {
                every.tick().await;
                let mcp = upkeep.clone();
                let _ = tokio::task::spawn_blocking(move || mcp.maintain()).await;
            }
        });
        axum::serve(listener, routes::router(server)).with_graceful_shutdown(shutdown()).await.map_err(|e| format!("the server stopped ({e})"))
    })
}

/// Resolves on Ctrl+C or, on Unix, SIGTERM (`docker stop`).
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
    log::info!("stopping");
}

/// `--healthcheck`: asks the running server's `/healthz` (the image has no curl).
fn healthcheck() -> bool {
    use std::io::{Read, Write};
    let listen = env("ARTCRAFT_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into());
    let Ok(mut addr) = listen.parse::<SocketAddr>() else { return false };
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    if stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut answer = [0u8; 12];
    stream.read_exact(&mut answer).is_ok() && answer.starts_with(b"HTTP/1.1 200")
}

/// Log lines on stderr, which Docker collects. `ARTCRAFT_LOG` sets the level (default `info`).
struct Logger(log::LevelFilter);

impl Logger {
    fn install() {
        let level = env("ARTCRAFT_LOG").and_then(|l| l.parse().ok()).unwrap_or(log::LevelFilter::Info);
        if log::set_logger(Box::leak(Box::new(Logger(level)))).is_ok() {
            log::set_max_level(level);
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.0 && (metadata.target().starts_with("artcraft") || metadata.level() <= log::Level::Warn)
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{:<5} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}
