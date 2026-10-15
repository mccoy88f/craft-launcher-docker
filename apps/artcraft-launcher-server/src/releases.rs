//! The Craft Apps' latest releases, looked up by the server for every visitor: one round of GitHub
//! requests per `ARTCRAFT_CHECK_MINUTES` instead of one per visitor (GitHub allows 60 an hour per
//! address without a token). Answers are revalidated with ETags and saved across restarts by the
//! desktop launcher's own [`GitHubSource`].

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use artcraft_launcher_core::catalog::{self, SUITE};
use artcraft_launcher_core::hosted::{HostedRelease, Releases as Answer};
use artcraft_launcher_core::protocol::{LatestRelease, Source};
use artcraft_launcher_core::release::Release;
use artcraft_launcher_native::http::Http;
use artcraft_launcher_native::source::{GitHubSource, ReleaseSource};

/// How soon the administrator's Refresh may ask GitHub again.
const FORCE_GAP: Duration = Duration::from_secs(60);

struct Cached {
    answer: Answer,
    at: Instant,
}

pub struct Releases {
    source: Arc<GitHubSource>,
    every: Duration,
    /// Stable releases, then with pre-releases. The lock also keeps two lookups from running at once.
    cache: tokio::sync::Mutex<[Option<Cached>; 2]>,
}

impl Releases {
    pub fn new(http: Http, data: &Path, every: Duration) -> Self {
        let source = Arc::new(GitHubSource::new(http, data.join("cache").join("releases.json")));
        Self { source, every, cache: tokio::sync::Mutex::new([None, None]) }
    }

    /// The latest releases, looked up again when the saved ones are older than the check interval
    /// (or, with `force`, a minute).
    pub async fn get(&self, prereleases: bool, force: bool) -> Answer {
        let mut cache = self.cache.lock().await;
        let slot = &mut cache[usize::from(prereleases)];
        if let Some(c) = slot.as_ref() {
            let age = c.at.elapsed();
            if age < if force { FORCE_GAP } else { self.every } {
                return c.answer.clone();
            }
        }
        let source = self.source.clone();
        let answer = match tokio::task::spawn_blocking(move || look_up(&*source, prereleases)).await {
            Ok(a) => a,
            Err(e) => {
                log::warn!("the release lookup stopped ({e})");
                return slot.as_ref().map_or_else(|| failed("the release lookup stopped"), |c| c.answer.clone());
            }
        };
        *slot = Some(Cached { answer: answer.clone(), at: Instant::now() });
        answer
    }

    /// The latest release of one app, for an install.
    pub async fn latest(&self, app: &str, prereleases: bool) -> Result<Release, String> {
        let answer = self.get(prereleases, false).await;
        match answer.apps.into_iter().find(|a| a.id == app).map(|a| a.latest) {
            Some(LatestRelease::Found(r)) => Ok(r),
            Some(LatestRelease::None) => Err("this app has no release yet".into()),
            Some(LatestRelease::Failed(why)) => Err(format!("couldn't look up the latest release ({why})")),
            None => Err("no such app".into()),
        }
    }
}

/// Asks GitHub about every app and the launcher itself (blocking: run off the async threads).
fn look_up(source: &dyn ReleaseSource, prereleases: bool) -> Answer {
    let mut used = Source::GitHubApi;
    let apps = SUITE
        .iter()
        .map(|app| {
            let answer = source.latest(app.repo, prereleases);
            if matches!(answer.latest, LatestRelease::Found(_)) {
                used = answer.source;
            }
            HostedRelease { id: app.id.to_string(), latest: answer.latest }
        })
        .collect();
    let launcher = match source.latest(catalog::LAUNCHER_REPO, false).latest {
        LatestRelease::Found(r) => Some(r),
        _ => None,
    };
    source.save();
    Answer { apps, launcher, checked_at: artcraft_launcher_native::now(), source: used, rate_limited_until: source.rate_limited_until() }
}

fn failed(why: &str) -> Answer {
    Answer {
        apps: SUITE.iter().map(|a| HostedRelease { id: a.id.to_string(), latest: LatestRelease::Failed(why.into()) }).collect(),
        launcher: None,
        checked_at: artcraft_launcher_native::now(),
        source: Source::GitHubApi,
        rate_limited_until: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artcraft_launcher_native::source::Answer as SourceAnswer;

    struct Fake;

    impl ReleaseSource for Fake {
        fn latest(&self, repo: &str, _prereleases: bool) -> SourceAnswer {
            let latest = if repo == "storytold/photocraft" {
                LatestRelease::Found(Release { tag: "v0.8.2".into(), ..Release::default() })
            } else {
                LatestRelease::None
            };
            SourceAnswer { latest, source: Source::GitHubPages }
        }
        fn cached(&self, _repo: &str, _prereleases: bool) -> Option<LatestRelease> {
            None
        }
        fn rate_limited_until(&self) -> Option<i64> {
            Some(42)
        }
        fn save(&self) {}
    }

    #[test]
    fn answers_for_every_app() {
        let answer = look_up(&Fake, false);
        assert_eq!(answer.apps.len(), SUITE.len());
        assert!(matches!(&answer.apps[0].latest, LatestRelease::Found(r) if r.tag == "v0.8.2"));
        assert_eq!(answer.source, Source::GitHubPages);
        assert_eq!(answer.rate_limited_until, Some(42));
        assert_eq!(answer.launcher, None);
    }
}
