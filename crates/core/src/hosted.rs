//! The HTTP API of a launcher server (`apps/artcraft-launcher-server`), shared with the browser
//! build that talks to it, so the two ends always agree on the shapes.
//!
//! A launcher server hosts the Craft Apps' browser builds (`<app>-web-<version>.zip`) at
//! `apps/<app>/`. Anyone can open them; only its administrator, signed in under `admin/`, can
//! install, update or remove them.
//!
//! | Request | Who | Answer |
//! |---|---|---|
//! | `GET api/state` | everyone (`admin/api/state`: the administrator) | [`ServerState`] |
//! | `GET api/releases[?pre=1][&force=1]` | everyone; `force` only under `admin/` | [`Releases`] |
//! | `POST admin/api/apps/<app>/<action>[?pre=1]` | the administrator | [`ServerState`] |
//!
//! | `POST admin/api/mcp` with a [`McpChange`] | the administrator | [`McpAnswer`] |
//! | `POST`, `DELETE mcp/<app>` | an MCP client with an access token | MCP (Streamable HTTP) |
//!
//! `<action>` is `install`, `uninstall`, `pause` or `cancel`. Every `POST` under `admin/` carries
//! the [`REQUEST_HEADER`] header, which a form on another site can't send.
//!
//! ## MCP
//!
//! The Craft Apps' command-line tools (`<app>-cli`, from the app's Linux release) are MCP servers
//! over stdio. The launcher server runs one per MCP session and speaks MCP's Streamable HTTP
//! transport at `mcp/<app>` for it, for each app the administrator turned on. Clients sign in
//! with an access token (`Authorization: Bearer …`); each app works in a folder of its own on
//! the server, emptied of old files on a schedule and capped in size.

use serde::{Deserialize, Serialize};

use crate::protocol::{JobPhase, LatestRelease, PausedDownload, Source};
use crate::release::Release;
use crate::status::Installed;

/// Sent with every state-changing request (any value), so a page on another site can't make the
/// administrator's browser install or remove an app: a cross-site form can't set a header, and a
/// cross-site `fetch` with one needs a CORS permission the server never gives.
pub const REQUEST_HEADER: &str = "x-artcraft-launcher";

/// What the server has installed and is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerState {
    /// The page asking is the administrator's: it may install, update and remove apps.
    pub admin: bool,
    /// Where the apps are, relative to the page that asked: `apps/` or `../apps/`.
    pub apps_url: String,
    pub apps: Vec<HostedApp>,
    /// The MCP server's settings and tokens (the administrator's page only).
    #[serde(default)]
    pub mcp: Option<McpState>,
}

impl ServerState {
    /// The address of an installed app, relative to the page that asked.
    pub fn app_url(&self, app: &str) -> String {
        format!("{}{app}/", self.apps_url)
    }
}

/// One app on the server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedApp {
    pub id: String,
    pub installed: Option<Installed>,
    pub job: Option<JobPhase>,
    pub paused: Option<PausedDownload>,
    /// How the last job ended.
    pub finished: Option<FinishedJob>,
}

/// How a job ended. `seq` grows with every job the server runs, so a page that polls sees each
/// ending once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedJob {
    pub seq: u64,
    pub result: Result<String, String>,
}

/// The latest releases, as the server last looked them up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Releases {
    pub apps: Vec<HostedRelease>,
    /// The latest ArtCraft Launcher release.
    pub launcher: Option<Release>,
    /// Unix seconds.
    pub checked_at: i64,
    pub source: Source,
    pub rate_limited_until: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedRelease {
    pub id: String,
    pub latest: LatestRelease,
}

/// The MCP server, as the administrator sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpState {
    /// The main switch: off, no app can be reached.
    pub enabled: bool,
    /// Files in a work folder not changed for this many days are deleted (0: never).
    pub retention_days: u32,
    /// The most a work folder may hold, in bytes; past it, tool calls are refused.
    pub quota_bytes: u64,
    /// MCP sessions running at once, across apps.
    pub max_sessions: u32,
    /// Where the MCP endpoints are, relative to the page that asked (`../mcp/`); the browser
    /// turns it into a full address.
    pub base_url: String,
    pub apps: Vec<McpApp>,
    pub tokens: Vec<McpToken>,
}

impl McpState {
    pub fn app(&self, id: &str) -> Option<&McpApp> {
        self.apps.iter().find(|a| a.id == id)
    }
}

/// One app's MCP server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpApp {
    pub id: String,
    /// The administrator turned it on.
    pub enabled: bool,
    /// Its command-line tool is installed (it comes with the app's Linux release).
    pub ready: bool,
    /// The work folder, as the app sees it.
    pub work_dir: String,
    /// Bytes in the work folder.
    pub used_bytes: u64,
    /// Sessions running now.
    pub sessions: u32,
}

/// An access token, without its secret (shown once, when it's made).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToken {
    pub id: String,
    pub name: String,
    /// The last characters of the token, to tell tokens apart.
    pub hint: String,
    /// The apps it may use; `None`: every app that's on.
    pub apps: Option<Vec<String>>,
    /// Unix seconds.
    pub created_at: i64,
    pub last_used: Option<i64>,
}

/// A change to the MCP server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum McpChange {
    Enabled {
        on: bool,
    },
    RetentionDays {
        days: u32,
    },
    QuotaBytes {
        bytes: u64,
    },
    MaxSessions {
        sessions: u32,
    },
    /// Turn one app's MCP server on (fetching its command-line tool if needed) or off.
    App {
        app: String,
        on: bool,
    },
    /// Delete everything in an app's work folder (ending its sessions).
    Empty {
        app: String,
    },
    CreateToken {
        name: String,
        apps: Option<Vec<String>>,
    },
    RevokeToken {
        id: String,
    },
}

/// The answer to a [`McpChange`]: the new state, and a new token's secret, once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAnswer {
    pub state: ServerState,
    pub token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::InstallSource;

    #[test]
    fn state_round_trips_through_json() {
        let state = ServerState {
            admin: false,
            apps_url: "../apps/".into(),
            apps: vec![HostedApp {
                id: "photocraft".into(),
                installed: Some(Installed {
                    version: crate::version::parse("0.8.2"),
                    source: InstallSource::Launcher,
                    location: "apps/photocraft/".into(),
                    package: None,
                }),
                job: Some(JobPhase::Downloading { done: 1, total: 2 }),
                paused: None,
                finished: Some(FinishedJob { seq: 3, result: Err("no space left".into()) }),
            }],
            mcp: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<ServerState>(&json).unwrap(), state);
        assert_eq!(state.app_url("photocraft"), "../apps/photocraft/");
    }

    #[test]
    fn mcp_changes_are_tagged() {
        let change = McpChange::App { app: "photocraft".into(), on: true };
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(json, r#"{"change":"app","app":"photocraft","on":true}"#);
        assert_eq!(serde_json::from_str::<McpChange>(&json).unwrap(), change);
        // A server without MCP answers states without the field.
        let state: ServerState = serde_json::from_str(r#"{"admin":false,"apps_url":"apps/","apps":[]}"#).unwrap();
        assert_eq!(state.mcp, None);
    }
}
