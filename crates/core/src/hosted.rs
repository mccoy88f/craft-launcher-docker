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
//! `<action>` is `install`, `uninstall`, `pause` or `cancel`. Every `POST` carries the
//! [`REQUEST_HEADER`] header, which a form on another site can't send.

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
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<ServerState>(&json).unwrap(), state);
        assert_eq!(state.app_url("photocraft"), "../apps/photocraft/");
    }
}
