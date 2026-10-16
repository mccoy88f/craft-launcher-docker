//! Building an app's MCP tool from its source code, for an app whose release's tool can't be
//! confined to a work folder but whose code can ([`FROM_SOURCE`]).
//!
//! The launcher has no compiler. A separate service, `cli-builder` (`packaging/builder/`), runs
//! next to it with the official Rust image and sees only a shared folder, never the launcher's
//! data or tokens:
//!
//! ```text
//! <builds>/builder.alive          Unix seconds, rewritten every 20 s while the service runs
//! <builds>/requests/<app>         the launcher asks for a build (empty file)
//! <builds>/status/<app>.json      {"state": "queued|building|done|failed", "commit": "…"}
//! <builds>/status/<app>.log       the build's output
//! <builds>/out/<app>/<commit>/<app>-cli
//! ```
//!
//! The service clones `https://github.com/storytold/<app>` (the URL is its own, from the app's
//! id) and runs `cargo build --release --locked -p <app>-cli`. The launcher copies the result
//! into the app's folder and runs it only if it confines itself, like a release's tool.

use std::path::{Path, PathBuf};

use artcraft_launcher_core::catalog::CraftApp;
use artcraft_launcher_core::hosted::SourceBuild;
use serde::Deserialize;

/// Apps whose code on `main` confines the MCP tool (`--automation-read-root` and
/// `--automation-write-root`) while their latest release doesn't. Add an app here when that's
/// so; the release's tool is used again as soon as one can be confined.
pub const FROM_SOURCE: [&str; 2] = ["deckcraft", "designcraft"];

/// The build service counts as running when it wrote its heartbeat this recently (seconds).
const ALIVE: i64 = 90;

pub struct Builds {
    dir: PathBuf,
}

#[derive(Deserialize)]
struct StatusFile {
    state: String,
    #[serde(default)]
    commit: Option<String>,
}

/// What the build service last said about an app.
pub struct Status {
    pub state: SourceBuild,
    /// Set once a build finished.
    pub done: bool,
    pub commit: Option<String>,
}

impl Builds {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Whether the build service is running.
    pub fn alive(&self) -> bool {
        std::fs::read_to_string(self.dir.join("builder.alive"))
            .ok()
            .and_then(|t| t.trim().parse::<i64>().ok())
            .is_some_and(|at| (artcraft_launcher_native::now() - at).abs() < ALIVE)
    }

    /// Asks the build service to build `app`'s tool from its source.
    pub fn request(&self, app: &CraftApp) -> Result<(), String> {
        if !FROM_SOURCE.contains(&app.id) {
            return Err(format!("{}'s source code can't confine its MCP tool yet either", app.name));
        }
        if !self.alive() {
            return Err("the build service (cli-builder) isn't running: add it to the Docker Compose file".into());
        }
        for sub in ["requests", "status"] {
            std::fs::create_dir_all(self.dir.join(sub)).map_err(|e| format!("couldn't write to {} ({e})", self.dir.display()))?;
        }
        artcraft_launcher_native::paths::write_atomic(&self.dir.join("status").join(format!("{}.json", app.id)), br#"{"state":"queued"}"#)?;
        std::fs::write(self.dir.join("requests").join(app.id), b"").map_err(|e| format!("couldn't ask for the build ({e})"))?;
        log::info!("{}: asked the build service to build its MCP tool from source", app.name);
        Ok(())
    }

    /// What the build service last said about `app`, if anything.
    pub fn status(&self, app: &str) -> Option<Status> {
        let text = std::fs::read(self.dir.join("status").join(format!("{app}.json"))).ok()?;
        let file: StatusFile = serde_json::from_slice(&text).ok()?;
        let commit = file.commit.filter(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()));
        let (state, done) = match file.state.as_str() {
            "queued" => (SourceBuild::Queued, false),
            "building" => (SourceBuild::Building, false),
            "failed" => (SourceBuild::Failed, false),
            "done" => (SourceBuild::Idle, true),
            _ => return None,
        };
        Some(Status { state, done, commit })
    }

    /// The last lines of the build's output, to say why it failed.
    pub fn why_failed(&self, app: &str) -> String {
        let log = std::fs::read_to_string(self.dir.join("status").join(format!("{app}.log"))).unwrap_or_default();
        let lines: Vec<&str> = log.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        // cargo's own summary ("error: could not compile …") is the most telling line.
        let line = lines.iter().rev().find(|l| l.starts_with("error")).or(lines.last()).copied().unwrap_or("no output");
        line.chars().take(300).collect()
    }

    /// The tool a finished build made.
    pub fn output(&self, app: &CraftApp, commit: &str) -> Option<PathBuf> {
        let name = format!("{}-cli", app.stems().next().unwrap_or(app.id));
        let path = self.dir.join("out").join(app.id).join(commit).join(name);
        path.is_file().then_some(path)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_the_build_service_says() {
        let dir = tempfile::tempdir().unwrap();
        let builds = Builds::new(dir.path().to_path_buf());
        let deck = CraftApp::by_id("deckcraft").unwrap();
        assert!(!builds.alive());
        assert!(builds.request(deck).is_err(), "no build service, no request");
        std::fs::write(dir.path().join("builder.alive"), artcraft_launcher_native::now().to_string()).unwrap();
        builds.request(deck).unwrap();
        assert!(dir.path().join("requests/deckcraft").exists());
        assert_eq!(builds.status("deckcraft").unwrap().state, SourceBuild::Queued);
        assert!(builds.request(CraftApp::by_id("filmcraft").unwrap()).is_err(), "only apps whose source can confine");

        let commit = "1dc6972a3b4c5d6e7f8091a2b3c4d5e6f7081920";
        std::fs::write(dir.path().join("status/deckcraft.json"), format!(r#"{{"state":"done","commit":"{commit}"}}"#)).unwrap();
        let status = builds.status("deckcraft").unwrap();
        assert!(status.done && status.commit.as_deref() == Some(commit));
        assert!(builds.output(deck, commit).is_none());
        std::fs::create_dir_all(dir.path().join("out/deckcraft").join(commit)).unwrap();
        std::fs::write(dir.path().join("out/deckcraft").join(commit).join("deckcraft-cli"), b"x").unwrap();
        assert!(builds.output(deck, commit).is_some());

        std::fs::write(dir.path().join("status/deckcraft.json"), r#"{"state":"failed","commit":"../../etc"}"#).unwrap();
        std::fs::write(dir.path().join("status/deckcraft.log"), "Compiling x\nerror: could not compile `deckcraft-engine`\nwarning: build failed\n")
            .unwrap();
        let status = builds.status("deckcraft").unwrap();
        assert_eq!((status.state, status.commit), (SourceBuild::Failed, None));
        assert_eq!(builds.why_failed("deckcraft"), "error: could not compile `deckcraft-engine`");
    }
}
