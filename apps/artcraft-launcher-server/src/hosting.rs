//! The Craft Apps the server hosts: installing an app's browser build, updating and removing it,
//! and knowing which folder serves it.
//!
//! Layout under the data folder:
//!
//! ```text
//! apps/<app>/installed.json   which version is installed, and in which folder
//! apps/<app>/<version>-<n>/   the unpacked site (index.html at its top)
//! cache/downloads/            zips being downloaded (a paused one resumes from here)
//! ```
//!
//! An update unpacks next to the running version and switches `installed.json` over in one
//! rename, so visitors never see half an app. The previous folder is kept until the next update,
//! for pages that are still loading its files.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::catalog::{CraftApp, SUITE};
use artcraft_launcher_core::hosted::{FinishedJob, HostedApp, ServerState};
use artcraft_launcher_core::platform::web_bundle;
use artcraft_launcher_core::protocol::{CANCELLED, JobPhase, PAUSED, PausedDownload};
use artcraft_launcher_core::release::{self, Release};
use artcraft_launcher_core::status::{InstallSource, Installed};
use artcraft_launcher_core::version;
use artcraft_launcher_native::http::{self, Http, Stop, Transfer};
use serde::{Deserialize, Serialize};

/// An unpacked app may not be bigger than this (PhotoCraft's site is about 12 MB).
const MAX_UNPACKED: u64 = 2 << 30;
/// Nor have more files than this.
const MAX_FILES: usize = 50_000;
const RECORD: &str = "installed.json";

/// What `installed.json` says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// `0.8.2`, or the tag when it isn't a version.
    pub version: String,
    pub tag: String,
    /// The folder under `apps/<app>/` that holds the site.
    pub dir: String,
    /// The zip it came from.
    pub asset: String,
    /// Unix seconds.
    pub installed_at: i64,
}

/// A download kept for later: paused, or stopped by a lost connection.
#[derive(Clone, Debug)]
struct Kept {
    file: PathBuf,
    size: u64,
}

#[derive(Default)]
struct Slot {
    record: Option<Record>,
    job: Option<JobPhase>,
    stop: Option<Arc<Stop>>,
    kept: Option<Kept>,
    finished: Option<FinishedJob>,
}

#[derive(Default)]
struct Inner {
    apps: BTreeMap<&'static str, Slot>,
    seq: u64,
}

pub struct Hosting {
    apps_dir: PathBuf,
    downloads: PathBuf,
    http: Http,
    inner: Mutex<Inner>,
}

impl Hosting {
    /// Reads what's installed under `data`.
    pub fn open(data: &Path, http: Http) -> Result<Self, String> {
        let apps_dir = data.join("apps");
        let downloads = data.join("cache").join("downloads");
        for dir in [&apps_dir, &downloads] {
            std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
        }
        // Downloads paused long ago are for releases long since replaced.
        http::clean_downloads(&downloads, Duration::from_secs(7 * 24 * 3600), Duration::from_secs(24 * 3600));
        let mut inner = Inner::default();
        for app in &SUITE {
            let record = read_record(&apps_dir.join(app.id));
            if let Some(r) = &record {
                log::info!("{} {} is installed", app.name, r.version);
            }
            inner.apps.insert(app.id, Slot { record, ..Slot::default() });
        }
        Ok(Self { apps_dir, downloads, http, inner: Mutex::new(inner) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What's installed and going on, for a page whose apps are at `apps_url`.
    pub fn state(&self, admin: bool, apps_url: &str) -> ServerState {
        let inner = self.lock();
        let apps = SUITE
            .iter()
            .map(|app| {
                let slot = inner.apps.get(app.id);
                HostedApp {
                    id: app.id.to_string(),
                    installed: slot.and_then(|s| s.record.as_ref()).map(|r| Installed {
                        version: version::parse(&r.version),
                        source: InstallSource::Launcher,
                        location: format!("{apps_url}{}/", app.id),
                        package: None,
                    }),
                    job: slot.and_then(|s| s.job),
                    paused: slot
                        .and_then(|s| s.kept.as_ref())
                        .filter(|_| slot.is_some_and(|s| s.job.is_none()))
                        .map(|k| PausedDownload { done: http::partial_bytes(&k.file, k.size).unwrap_or(0), total: k.size }),
                    finished: slot.and_then(|s| s.finished.clone()),
                }
            })
            .collect();
        ServerState { admin, apps_url: apps_url.to_string(), apps }
    }

    /// The folder serving `app`, when it's installed.
    pub fn site(&self, app: &str) -> Option<PathBuf> {
        let inner = self.lock();
        let record = inner.apps.get(app)?.record.as_ref()?;
        Some(self.apps_dir.join(app).join(&record.dir))
    }

    /// Starts installing (or updating to) `release` of `app` on a worker thread.
    pub fn install(self: &Arc<Self>, app: &'static CraftApp, release: Release) -> Result<(), String> {
        let stop = self.begin(app, JobPhase::Downloading { done: 0, total: 0 })?;
        let this = self.clone();
        std::thread::spawn(move || {
            let mut kept = None;
            let result = this.run_install(app, &release, &stop, &mut kept);
            this.finish(app, result, kept);
        });
        Ok(())
    }

    /// Starts removing `app` on a worker thread.
    pub fn uninstall(self: &Arc<Self>, app: &'static CraftApp) -> Result<(), String> {
        if self.lock().apps.get(app.id).is_none_or(|s| s.record.is_none()) {
            return Err(format!("{} isn't installed", app.name));
        }
        self.begin(app, JobPhase::Removing)?;
        let this = self.clone();
        std::thread::spawn(move || {
            let dir = this.apps_dir.join(app.id);
            let result = std::fs::remove_dir_all(&dir).map_err(|e| format!("couldn't remove {} ({e})", dir.display()));
            if result.is_ok() {
                if let Some(slot) = this.lock().apps.get_mut(app.id) {
                    slot.record = None;
                }
                log::info!("{} was removed", app.name);
            }
            this.finish(app, result.map(|()| format!("{} was removed", app.name)), None);
        });
        Ok(())
    }

    /// Pauses a running download, keeping what's downloaded so far.
    pub fn pause(&self, app: &str) {
        if let Some(stop) = self.lock().apps.get(app).and_then(|s| s.stop.clone()) {
            stop.pause();
        }
    }

    /// Stops a running download, or throws away a paused one.
    pub fn cancel(&self, app: &str) {
        let mut inner = self.lock();
        let Some(slot) = inner.apps.get_mut(app) else { return };
        if let Some(stop) = &slot.stop {
            stop.cancel();
        } else if let Some(kept) = slot.kept.take() {
            http::discard(&kept.file, kept.size);
        }
    }

    /// Marks `app` busy, unless it already is.
    fn begin(&self, app: &CraftApp, phase: JobPhase) -> Result<Arc<Stop>, String> {
        let mut inner = self.lock();
        let slot = inner.apps.entry(app.id).or_default();
        if slot.job.is_some() {
            return Err(format!("{} is busy with another job", app.name));
        }
        let stop = Arc::new(Stop::default());
        slot.job = Some(phase);
        slot.stop = Some(stop.clone());
        Ok(stop)
    }

    fn set_phase(&self, app: &str, phase: JobPhase) {
        if let Some(slot) = self.lock().apps.get_mut(app) {
            slot.job = Some(phase);
        }
    }

    fn finish(&self, app: &CraftApp, result: Result<String, String>, kept: Option<Kept>) {
        if let Err(e) = &result
            && e != PAUSED
            && e != CANCELLED
        {
            log::warn!("{}: {e}", app.name);
        }
        let mut inner = self.lock();
        inner.seq += 1;
        let seq = inner.seq;
        if let Some(slot) = inner.apps.get_mut(app.id) {
            slot.job = None;
            slot.stop = None;
            // A paused download, or one a lost connection stopped (which ends paused too), resumes
            // on the next install; after any other ending there's nothing worth keeping.
            let paused = matches!(&result, Err(e) if e == PAUSED);
            slot.kept = match kept {
                Some(k) if paused && (http::partial_bytes(&k.file, k.size).is_some() || http::finished(&k.file, k.size)) => Some(k),
                Some(k) => {
                    http::discard(&k.file, k.size);
                    None
                }
                None => None,
            };
            slot.finished = Some(FinishedJob { seq, result });
        }
    }

    fn run_install(&self, app: &'static CraftApp, release: &Release, stop: &Stop, kept: &mut Option<Kept>) -> Result<String, String> {
        let stems: Vec<&str> = app.stems().collect();
        let label = release.version_label();
        let asset = web_bundle(release, &stems).ok_or_else(|| format!("{} {label} has no browser version", app.name))?;
        let sha256 = match &asset.sha256 {
            Some(h) => h.clone(),
            None => self.published_checksum(release, &asset.name)?,
        };
        let file = self.downloads.join(&asset.name);
        *kept = Some(Kept { file: file.clone(), size: asset.size });
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut progress = |t: Transfer| {
            // Visitors poll about once a second; a few updates a second are plenty.
            if last.elapsed() >= Duration::from_millis(200) {
                last = Instant::now();
                self.set_phase(
                    app.id,
                    match t {
                        Transfer::Bytes { done, total } => JobPhase::Downloading { done, total },
                        Transfer::Reconnecting { done, total } => JobPhase::Reconnecting { done, total },
                    },
                );
            }
        };
        self.http.download(&asset.url, &file, asset.size, &sha256, stop, &mut progress)?;
        stop.check()?;
        self.set_phase(app.id, JobPhase::Installing);

        let app_dir = self.apps_dir.join(app.id);
        let previous = read_record(&app_dir);
        let dir = folder_name(&label, &app_dir);
        let staging = app_dir.join(format!(".unpacking-{dir}"));
        let _ = std::fs::remove_dir_all(&staging);
        if let Err(e) = unpack(&file, &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            // The zip matched its published checksum, so fetching it again won't help.
            return Err(e);
        }
        let target = app_dir.join(&dir);
        std::fs::rename(&staging, &target).map_err(|e| format!("couldn't move the app into place ({e})"))?;
        let record = Record {
            version: release.version.as_ref().map_or_else(|| release.tag.clone(), ToString::to_string),
            tag: release.tag.clone(),
            dir: dir.clone(),
            asset: asset.name.clone(),
            installed_at: artcraft_launcher_native::now(),
        };
        let json = serde_json::to_vec_pretty(&record).map_err(|e| format!("couldn't write the install record ({e})"))?;
        artcraft_launcher_native::paths::write_atomic(&app_dir.join(RECORD), &json)?;
        if let Some(slot) = self.lock().apps.get_mut(app.id) {
            slot.record = Some(record);
        }
        // Keep the folder visitors may still be loading from; older ones go.
        prune(&app_dir, &[dir.as_str(), previous.as_ref().map_or("", |p| p.dir.as_str())]);
        let _ = std::fs::remove_file(&file);
        *kept = None;
        let verb = if previous.is_some() { "updated to" } else { "installed:" };
        log::info!("{} {verb} {label}", app.name);
        Ok(format!("{} {verb} {label}", app.name))
    }

    /// The checksum `SHA256SUMS.txt` publishes for `name`. Nothing is installed without one.
    fn published_checksum(&self, release: &Release, name: &str) -> Result<String, String> {
        let missing = || format!("no checksum is published for {name}, so it wasn't installed");
        let sums = release.sums_asset().ok_or_else(missing)?;
        let text = self.http.get_text(&sums.url)?.ok_or_else(missing)?;
        release::parse_sha256sums(&text).into_iter().find(|(_, n)| n == name).map(|(h, _)| h).ok_or_else(missing)
    }
}

fn read_record(app_dir: &Path) -> Option<Record> {
    let bytes = std::fs::read(app_dir.join(RECORD)).ok()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    // Only a plain folder name of our own, which still holds the site.
    let plain = !record.dir.is_empty() && !record.dir.starts_with('.') && Path::new(&record.dir).components().count() == 1;
    (plain && app_dir.join(&record.dir).join("index.html").is_file()).then_some(record)
}

/// A new folder for `label`: `0.8.2-1`, `0.8.2-2`… whichever is free.
fn folder_name(label: &str, app_dir: &Path) -> String {
    let safe: String = label.chars().map(|c| if c.is_ascii_alphanumeric() || "._+-".contains(c) { c } else { '_' }).take(64).collect();
    let safe = safe.trim_start_matches('.');
    let safe = if safe.is_empty() { "app" } else { safe };
    (1u32..).map(|n| format!("{safe}-{n}")).find(|name| !app_dir.join(name).exists()).unwrap_or_else(|| safe.to_string())
}

/// Removes every version folder in `app_dir` but `keep` (and leftovers of unfinished unpacking).
fn prune(app_dir: &Path, keep: &[&str]) {
    let Ok(entries) = std::fs::read_dir(app_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == RECORD || keep.contains(&name.as_str()) || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(entry.path()) {
            log::warn!("couldn't remove {} ({e})", entry.path().display());
        }
    }
}

/// Unpacks a browser build into `dest`. The zip holds the site in one top folder
/// (`photocraft-web-0.8.2/`), which is dropped. Paths that would land outside `dest`, links,
/// and archives too big or with too many files are refused.
pub fn unpack(zip_file: &Path, dest: &Path) -> Result<(), String> {
    let bad = |e: zip::result::ZipError| format!("the download isn't a readable zip ({e})");
    let file = std::fs::File::open(zip_file).map_err(|e| format!("couldn't open {} ({e})", zip_file.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(bad)?;
    if archive.len() > MAX_FILES {
        return Err(format!("the browser version has more than {MAX_FILES} files"));
    }
    let mut names = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(bad)?;
        let name = entry.enclosed_name().ok_or_else(|| format!("the zip has an unsafe path: {}", entry.name()))?;
        if entry.unix_mode().is_some_and(|m| m & 0o170_000 == 0o120_000) {
            return Err(format!("the zip has a link, which isn't allowed: {}", entry.name()));
        }
        names.push(name);
    }
    let top = common_top(&names);
    std::fs::create_dir_all(dest).map_err(|e| format!("couldn't create {} ({e})", dest.display()))?;
    let mut total: u64 = 0;
    for (i, name) in names.iter().enumerate() {
        let relative = match &top {
            Some(t) => name.strip_prefix(t).unwrap_or(name),
            None => name.as_path(),
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(relative);
        let mut entry = archive.by_index(i).map_err(bad)?;
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("couldn't create {} ({e})", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("couldn't create {} ({e})", parent.display()))?;
        }
        let mut f = std::fs::File::create(&out).map_err(|e| format!("couldn't write {} ({e})", out.display()))?;
        let room = MAX_UNPACKED.saturating_sub(total);
        let written = std::io::copy(&mut std::io::Read::take(&mut entry, room.saturating_add(1)), &mut f)
            .map_err(|e| format!("couldn't unpack {} ({e})", relative.display()))?;
        total = total.saturating_add(written);
        if total > MAX_UNPACKED {
            return Err(format!("the browser version unpacks to more than {} MB", MAX_UNPACKED >> 20));
        }
    }
    if !dest.join("index.html").is_file() {
        return Err("the browser version has no index.html".into());
    }
    Ok(())
}

/// The one folder every entry is under, if there is one.
fn common_top(names: &[PathBuf]) -> Option<PathBuf> {
    let first = match names.first()?.components().next()? {
        Component::Normal(c) => PathBuf::from(c),
        _ => return None,
    };
    // The folder must hold everything, and something other than itself.
    let all_under = names.iter().all(|n| n.starts_with(&first));
    let deeper = names.iter().any(|n| n.components().count() > 1);
    (all_under && deeper).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut w = zip::ZipWriter::new(file.reopen().unwrap());
        for (name, data) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, zip::write::SimpleFileOptions::default()).unwrap();
            } else {
                w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap();
        file
    }

    #[test]
    fn unpacks_the_site_without_its_top_folder() {
        let z = zip(&[("photocraft-web-0.8.2/", b""), ("photocraft-web-0.8.2/index.html", b"<html>"), ("photocraft-web-0.8.2/app/parts.txt", b"a")]);
        let dest = tempfile::tempdir().unwrap();
        unpack(z.path(), &dest.path().join("site")).unwrap();
        assert_eq!(std::fs::read(dest.path().join("site/index.html")).unwrap(), b"<html>");
        assert!(dest.path().join("site/app/parts.txt").is_file());
    }

    #[test]
    fn unpacks_a_flat_site_too() {
        let z = zip(&[("index.html", b"<html>"), ("app.wasm", b"\0asm")]);
        let dest = tempfile::tempdir().unwrap();
        unpack(z.path(), dest.path()).unwrap();
        assert!(dest.path().join("app.wasm").is_file());
    }

    #[test]
    fn refuses_escapes_and_sites_without_a_page() {
        let dest = tempfile::tempdir().unwrap();
        let escape = zip(&[("../evil.html", b"x"), ("index.html", b"x")]);
        assert!(unpack(escape.path(), &dest.path().join("a")).unwrap_err().contains("unsafe path"));
        assert!(!dest.path().join("evil.html").exists());
        let no_page = zip(&[("site/readme.txt", b"x")]);
        assert!(unpack(no_page.path(), &dest.path().join("b")).unwrap_err().contains("index.html"));
        let not_zip = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(not_zip.path(), b"not a zip").unwrap();
        assert!(unpack(not_zip.path(), &dest.path().join("c")).is_err());
    }

    #[test]
    fn names_version_folders_and_prunes_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(folder_name("0.8.2", dir.path()), "0.8.2-1");
        std::fs::create_dir(dir.path().join("0.8.2-1")).unwrap();
        assert_eq!(folder_name("0.8.2", dir.path()), "0.8.2-2");
        assert_eq!(folder_name("../x y", dir.path()), "_x_y-1");
        for d in ["0.8.0-1", "0.8.1-1", ".unpacking-0.8.3-1"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        std::fs::write(dir.path().join(RECORD), b"{}").unwrap();
        prune(dir.path(), &["0.8.2-1", "0.8.1-1"]);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        left.sort();
        assert_eq!(left, ["0.8.1-1", "0.8.2-1", RECORD]);
    }

    #[test]
    fn reads_only_records_that_point_at_a_site() {
        let dir = tempfile::tempdir().unwrap();
        let record = |d: &str| {
            let r = Record { version: "0.8.2".into(), tag: "v0.8.2".into(), dir: d.into(), asset: "a.zip".into(), installed_at: 0 };
            std::fs::write(dir.path().join(RECORD), serde_json::to_vec(&r).unwrap()).unwrap();
        };
        record("0.8.2-1");
        assert_eq!(read_record(dir.path()), None);
        std::fs::create_dir(dir.path().join("0.8.2-1")).unwrap();
        std::fs::write(dir.path().join("0.8.2-1/index.html"), b"x").unwrap();
        assert_eq!(read_record(dir.path()).map(|r| r.dir), Some("0.8.2-1".into()));
        record("../0.8.2-1");
        assert_eq!(read_record(dir.path()), None);
    }
}
