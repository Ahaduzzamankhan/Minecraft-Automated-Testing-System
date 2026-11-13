//! Verified resource acquisition: parallel, resumable and cache-backed.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use mats_core::MatsHome;
use mats_core::config::Config;
use mats_core::fsutil;
use mats_core::types::{ResourceKind, ResourceRecord};
use serde_json::Value;

use crate::cache::{CacheStats, CachedMeta, CachedText, ObjectCache};
use crate::http::{HttpClient, NetError, local_path_from_url, read_local};

/// A resource that a test environment needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRequest {
    pub name: String,
    pub kind: ResourceKind,
    pub url: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

impl ResourceRequest {
    pub fn new(name: impl Into<String>, kind: ResourceKind, url: impl Into<String>) -> Self {
        ResourceRequest {
            name: name.into(),
            kind,
            url: url.into(),
            sha1: None,
            size: None,
        }
    }

    pub fn with_sha1(mut self, sha1: Option<String>) -> Self {
        self.sha1 = sha1;
        self
    }

    pub fn with_size(mut self, size: Option<u64>) -> Self {
        self.size = size;
        self
    }

    fn dedupe_key(&self) -> String {
        self.sha1
            .clone()
            .unwrap_or_else(|| format!("url:{}", self.url))
    }
}

/// A resource available in the local cache.
#[derive(Debug, Clone)]
pub struct FetchedResource {
    pub name: String,
    pub kind: ResourceKind,
    pub url: String,
    pub path: PathBuf,
    pub sha1: String,
    pub size: u64,
    pub from_cache: bool,
}

impl FetchedResource {
    pub fn record(&self) -> ResourceRecord {
        ResourceRecord {
            name: self.name.clone(),
            kind: self.kind,
            url: self.url.clone(),
            sha1: Some(self.sha1.clone()),
            size_bytes: Some(self.size),
            cache_path: self.path.display().to_string(),
            from_cache: self.from_cache,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Cached { size: u64 },
    Started { total: Option<u64> },
    Advanced { downloaded: u64, total: Option<u64> },
    Finished { size: u64, from_cache: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub name: String,
    pub kind: ResourceKind,
    pub event: ProgressEvent,
}

pub type ProgressFn = Arc<dyn Fn(&ProgressUpdate) + Send + Sync>;

/// Owns the HTTP client and the content-addressed cache.
#[derive(Clone)]
pub struct ResourceManager {
    http: HttpClient,
    cache: ObjectCache,
    meta_dir: PathBuf,
    max_parallel: usize,
    reporter: Option<ProgressFn>,
    offline: bool,
}

impl std::fmt::Debug for ResourceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceManager")
            .field("offline", &self.offline)
            .field("max_parallel", &self.max_parallel)
            .finish_non_exhaustive()
    }
}

impl ResourceManager {
    pub fn new(home: &MatsHome, config: &Config) -> Result<Self> {
        home.ensure_layout()?;
        Ok(ResourceManager {
            http: HttpClient::new(&config.network, config.runtime.offline),
            cache: ObjectCache::new(home),
            meta_dir: home.meta_dir(),
            max_parallel: config.runtime.max_parallel_downloads.clamp(1, 16),
            reporter: None,
            offline: config.runtime.offline,
        })
    }

    pub fn with_reporter(mut self, reporter: ProgressFn) -> Self {
        self.reporter = Some(reporter);
        self
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    pub fn cache(&self) -> &ObjectCache {
        &self.cache
    }

    pub fn offline(&self) -> bool {
        self.offline
    }

    /// Ensure one resource is present locally, downloading it when needed.
    pub fn fetch(&self, request: &ResourceRequest) -> Result<FetchedResource> {
        if let Some(expected) = &request.sha1 {
            if self.cache.contains(expected) {
                let path = self.cache.object_path(expected)?;
                let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                self.emit(&request.name, request.kind, ProgressEvent::Cached { size });
                return Ok(FetchedResource {
                    name: request.name.clone(),
                    kind: request.kind,
                    url: request.url.clone(),
                    path,
                    sha1: expected.clone(),
                    size,
                    from_cache: true,
                });
            }
        }

        if let Some(local) = local_path_from_url(&request.url) {
            return self.import_local(request, &local);
        }

        if self.offline {
            bail!(
                "offline mode: `{}` ({}) is not in the local cache",
                request.name,
                request.url
            );
        }

        let partial = self.cache.partial_path(&request.url)?;
        let started = Instant::now();
        let (downloaded, total) = self.stream_to_partial(request, &partial)?;
        if let Some(expected) = request.size {
            if downloaded != expected {
                let _ = fs::remove_file(&partial);
                bail!(
                    "size mismatch for `{}`: expected {expected} bytes, downloaded {downloaded}",
                    request.name
                );
            }
        }
        let actual = fsutil::sha1_file(&partial)
            .with_context(|| format!("hashing downloaded `{}`", request.name))?;
        if let Some(expected) = &request.sha1 {
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = fs::remove_file(&partial);
                bail!(
                    "SHA-1 verification failed for `{}`: metadata declares {expected}, downloaded content hashes to {actual}. The partial download was discarded.",
                    request.name
                );
            }
        }
        let object = self
            .cache
            .import(&partial, Some(&actual))
            .with_context(|| format!("storing `{}` in the cache", request.name))?;
        self.emit(
            &request.name,
            request.kind,
            ProgressEvent::Finished {
                size: downloaded,
                from_cache: false,
            },
        );
        tracing::debug!(
            name = %request.name,
            bytes = downloaded,
            total_reported = ?total,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "downloaded resource"
        );
        Ok(FetchedResource {
            name: request.name.clone(),
            kind: request.kind,
            url: request.url.clone(),
            path: object,
            sha1: actual,
            size: downloaded,
            from_cache: false,
        })
    }

    fn import_local(&self, request: &ResourceRequest, path: &Path) -> Result<FetchedResource> {
        let bytes = read_local(&path.to_path_buf()).map_err(|error| anyhow!("{error}"))?;
        let actual = fsutil::sha1_bytes(&bytes);
        if let Some(expected) = &request.sha1 {
            if !actual.eq_ignore_ascii_case(expected) {
                bail!(
                    "SHA-1 verification failed for local mirror `{}`: expected {expected}, got {actual}",
                    path.display()
                );
            }
        }
        let object = self.cache.store_bytes(&bytes)?;
        self.emit(
            &request.name,
            request.kind,
            ProgressEvent::Finished {
                size: bytes.len() as u64,
                from_cache: false,
            },
        );
        Ok(FetchedResource {
            name: request.name.clone(),
            kind: request.kind,
            url: request.url.clone(),
            path: object,
            sha1: actual,
            size: bytes.len() as u64,
            from_cache: false,
        })
    }

    fn stream_to_partial(
        &self,
        request: &ResourceRequest,
        partial: &Path,
    ) -> Result<(u64, Option<u64>)> {
        let existing = fs::metadata(partial).map(|meta| meta.len()).unwrap_or(0);
        let (mut reader, total, resumed) = match self.http.open_stream(&request.url, existing) {
            Ok(stream) => stream,
            Err(NetError::NotFound { .. }) => {
                bail!(
                    "official resource not found (HTTP 404): {} — the requested version or loader build may not exist",
                    request.url
                )
            }
            Err(NetError::RateLimited { reset, .. }) => bail!(
                "GitHub API rate limit reached while fetching `{}`; unauthenticated requests reset at {reset}. Retry later or install an update manually from the releases page.",
                request.name
            ),
            Err(error) => {
                return Err(anyhow!("downloading `{}`: {error}", request.name));
            }
        };
        if !resumed && existing > 0 {
            tracing::debug!(
                name = %request.name,
                "server does not support resume; restarting download"
            );
        }
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!resumed)
            .append(resumed)
            .open(partial)
            .with_context(|| format!("opening {}", partial.display()))?;
        let mut downloaded = if resumed { existing } else { 0 };
        let total = total.or(request.size);
        self.emit(
            &request.name,
            request.kind,
            ProgressEvent::Started { total },
        );
        let mut buffer = vec![0u8; 256 * 1024];
        let mut last_emit = Instant::now();
        loop {
            let read = reader.read(&mut buffer).with_context(|| {
                format!(
                    "reading response body for `{}` from {}",
                    request.name, request.url
                )
            })?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])
                .with_context(|| format!("writing {}", partial.display()))?;
            downloaded += read as u64;
            if last_emit.elapsed() >= Duration::from_millis(200) {
                last_emit = Instant::now();
                self.emit(
                    &request.name,
                    request.kind,
                    ProgressEvent::Advanced { downloaded, total },
                );
            }
        }
        file.flush().ok();
        file.sync_all().ok();
        Ok((downloaded, total))
    }

    /// Fetch many resources in parallel, deduplicating identical content.
    pub fn fetch_many(&self, requests: &[ResourceRequest]) -> Result<Vec<FetchedResource>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique: Vec<ResourceRequest> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut assignment: Vec<usize> = Vec::with_capacity(requests.len());
        for request in requests {
            let key = request.dedupe_key();
            match keys.iter().position(|existing| existing == &key) {
                Some(index) => assignment.push(index),
                None => {
                    keys.push(key);
                    assignment.push(unique.len());
                    unique.push(request.clone());
                }
            }
        }
        let task_count = unique.len();
        let workers = self.max_parallel.min(task_count).max(1);
        let queue = Mutex::new(VecDeque::from(
            unique.into_iter().enumerate().collect::<Vec<_>>(),
        ));
        let results: Mutex<Vec<Option<Result<FetchedResource>>>> =
            Mutex::new((0..task_count).map(|_| None).collect());

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let next = {
                            let mut guard = queue.lock().expect("download queue poisoned");
                            guard.pop_front()
                        };
                        let Some((index, request)) = next else {
                            break;
                        };
                        let result = self.fetch(&request);
                        let mut guard = results.lock().expect("download results poisoned");
                        guard[index] = Some(result);
                    }
                });
            }
        });

        let unique_results = results.into_inner().expect("download results poisoned");
        let mut resolved: Vec<FetchedResource> = Vec::with_capacity(task_count);
        let mut failures: Vec<String> = Vec::new();
        for entry in unique_results {
            match entry {
                Some(Ok(resource)) => resolved.push(resource),
                Some(Err(error)) => failures.push(format!("{error:#}")),
                None => failures.push("download task was never scheduled".to_string()),
            }
        }
        if !failures.is_empty() {
            let mut message = format!(
                "{} of {} resource downloads failed:",
                failures.len(),
                task_count
            );
            for failure in &failures {
                message.push_str("\n  - ");
                message.push_str(failure);
            }
            bail!(message);
        }
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        Ok(assignment
            .into_iter()
            .map(|index| {
                let mut resource = resolved[index].clone();
                // Only the first entry for a deduplicated resource is the actual
                // download; later entries were served from the same object.
                if !seen.insert(index) {
                    resource.from_cache = true;
                }
                resource
            })
            .collect())
    }

    /// Fetch JSON metadata with a disk cache honouring `ttl`.
    pub fn fetch_json(&self, url: &str, cache_key: &str, ttl: Duration) -> Result<Value> {
        let path = self.meta_path(cache_key);
        if let Some(entry) = self.read_meta(&path) {
            let age = now_unix().saturating_sub(entry.fetched_at);
            if self.http.offline() || age < ttl.as_secs() as i64 {
                return Ok(entry.body);
            }
        }
        if let Some(source) = url.strip_prefix("file://") {
            let text = fs::read_to_string(source)
                .with_context(|| format!("reading metadata from {url}"))?;
            let body: Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing metadata from {url}"))?;
            let entry = CachedMeta {
                url: url.to_string(),
                fetched_at: now_unix(),
                body: body.clone(),
            };
            if let Ok(bytes) = serde_json::to_vec(&entry) {
                let _ = fsutil::atomic_write(&self.meta_path(cache_key), &bytes);
            }
            return Ok(body);
        }
        if self.http.offline() {
            bail!("offline mode: cached metadata `{cache_key}` for {url} is missing or unreadable");
        }
        let body = match self.http.get_json(url) {
            Ok(body) => body,
            Err(error) if error.is_not_found() => {
                bail!("metadata not found (HTTP 404): {url}")
            }
            Err(error) => return Err(anyhow!("fetching metadata from {url}: {error}")),
        };
        let entry = CachedMeta {
            url: url.to_string(),
            fetched_at: now_unix(),
            body: body.clone(),
        };
        if let Ok(text) = serde_json::to_vec(&entry) {
            if let Err(error) = fsutil::atomic_write(&path, &text) {
                tracing::warn!("could not cache metadata {url}: {error}");
            }
        }
        Ok(body)
    }

    /// Fetch JSON metadata, always hitting the network when online.
    pub fn fetch_json_uncached(&self, url: &str) -> Result<Value> {
        if self.http.offline() {
            bail!("offline mode: cannot fetch {url}");
        }
        self.http
            .get_json(url)
            .map_err(|error| anyhow!("fetching {url}: {error}"))
    }

    /// Fetch text metadata (Maven XML, checksum files) with a disk cache.
    pub fn fetch_text(&self, url: &str, cache_key: &str, ttl: Duration) -> Result<String> {
        let path = self.meta_path(cache_key);
        if let Some(entry) = self.read_text_meta(&path) {
            let age = now_unix().saturating_sub(entry.fetched_at);
            if self.http.offline() || age < ttl.as_secs() as i64 {
                return Ok(entry.body);
            }
        }
        if self.http.offline() {
            bail!("offline mode: cached metadata `{cache_key}` for {url} is unavailable");
        }
        let body = self
            .http
            .get_text(url)
            .map_err(|error| anyhow!("fetching {url}: {error}"))?;
        let entry = CachedText {
            url: url.to_string(),
            fetched_at: now_unix(),
            body: body.clone(),
        };
        if let Ok(text) = serde_json::to_vec(&entry) {
            let _ = fsutil::atomic_write(&path, &text);
        }
        Ok(body)
    }

    /// Fetch a short checksum file (e.g. Maven `.sha1`), returning `None` on 404.
    pub fn try_fetch_text(&self, url: &str) -> Result<Option<String>> {
        if self.http.offline() {
            return Ok(None);
        }
        match self.http.get_text(url) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(anyhow!("fetching {url}: {error}")),
        }
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        let sanitized: String = key
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        self.meta_dir.join(format!("{sanitized}.json"))
    }

    fn read_meta(&self, path: &Path) -> Option<CachedMeta> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str::<CachedMeta>(&text).ok()
    }

    fn read_text_meta(&self, path: &Path) -> Option<CachedText> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str::<CachedText>(&text).ok()
    }

    fn emit(&self, name: &str, kind: ResourceKind, event: ProgressEvent) {
        if let Some(reporter) = &self.reporter {
            reporter(&ProgressUpdate {
                name: name.to_string(),
                kind,
                event,
            });
        }
    }

    /// Cache usage, including directories managed outside the object store.
    pub fn stats(&self, home: &MatsHome) -> CacheStats {
        let mut stats = self.cache.stats();
        stats.metas = count_files(&self.meta_dir);
        stats.java_runtimes = count_dirs(&home.java_dir());
        stats.environments = count_dirs(&home.envs_dir());
        stats
    }

    /// Remove cached resources. Metadata and environments are kept unless
    /// `include_environments` is set.
    pub fn clear(&self, home: &MatsHome, include_environments: bool) -> Result<u64> {
        let removed = self.cache.clear()?;
        let _ = fsutil::remove_dir_if_exists(&self.meta_dir);
        if include_environments {
            let _ = fsutil::remove_dir_if_exists(&home.envs_dir());
        }
        home.ensure_layout()?;
        Ok(removed)
    }
}

fn count_files(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .count() as u64
}

fn count_dirs(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .count() as u64
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Move or hard-link a cached object into a prepared environment.
///
/// Hard links avoid duplicating multi-hundred-megabyte JARs while keeping the
/// cache immutable; filesystems without hard links fall back to a copy.
pub fn materialize(source: &Path, target: &Path) -> Result<()> {
    if target.is_file() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    match fs::hard_link(source, target) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(source, target).with_context(|| {
                format!(
                    "copying cached object {} to {}",
                    source.display(),
                    target.display()
                )
            })?;
            Ok(())
        }
    }
}

/// A no-op progress reporter.
pub fn silent_reporter() -> Option<ProgressFn> {
    None
}

/// A reporter that prints one line per completed download.
pub fn stderr_reporter() -> ProgressFn {
    Arc::new(|update: &ProgressUpdate| match &update.event {
        ProgressEvent::Finished { size, from_cache } => {
            let source = if *from_cache { "cache" } else { "network" };
            eprintln!(
                "  {} {} ({}, {})",
                if *from_cache { "·" } else { "↓" },
                update.name,
                fsutil::human_bytes(*size),
                source
            );
        }
        ProgressEvent::Cached { size } => eprintln!(
            "  · {} ({}, cache)",
            update.name,
            fsutil::human_bytes(*size)
        ),
        _ => {}
    })
}

/// Open a file for reading with a length cap, used by checksum verification.
pub fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buffer = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit)
        .read_to_end(&mut buffer)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn manager(temp: &tempfile::TempDir) -> (MatsHome, ResourceManager) {
        let home = MatsHome::new(temp.path().join("home"));
        let mut config = Config::default();
        config.runtime.max_parallel_downloads = 4;
        let manager = ResourceManager::new(&home, &config).unwrap();
        (home, manager)
    }

    #[test]
    fn local_mirror_import_verifies_and_caches() {
        let temp = tempfile::tempdir().unwrap();
        let (_home, manager) = manager(&temp);
        let source = temp.path().join("lib.jar");
        fs::write(&source, b"jar-bytes").unwrap();
        let digest = fsutil::sha1_bytes(b"jar-bytes");
        let request = ResourceRequest::new(
            "lib",
            ResourceKind::Library,
            format!("file://{}", source.display()),
        )
        .with_sha1(Some(digest.clone()));
        let fetched = manager.fetch(&request).unwrap();
        assert_eq!(fetched.sha1, digest);
        assert_eq!(fetched.size, 9);
        assert!(!fetched.from_cache);
        assert!(manager.cache().contains(&digest));

        // Second fetch is served from the cache.
        let again = manager.fetch(&request).unwrap();
        assert!(again.from_cache);
    }

    #[test]
    fn local_mirror_checksum_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let (_home, manager) = manager(&temp);
        let source = temp.path().join("lib.jar");
        fs::write(&source, b"jar-bytes").unwrap();
        let request = ResourceRequest::new(
            "lib",
            ResourceKind::Library,
            format!("file://{}", source.display()),
        )
        .with_sha1(Some("0".repeat(40)));
        let error = manager.fetch(&request).unwrap_err();
        assert!(error.to_string().contains("SHA-1 verification failed"));
    }

    #[test]
    fn offline_mode_reports_missing_resources() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let mut config = Config::default();
        config.runtime.offline = true;
        let manager = ResourceManager::new(&home, &config).unwrap();
        let request = ResourceRequest::new(
            "client",
            ResourceKind::MinecraftClient,
            "https://example.invalid/client.jar",
        );
        let error = manager.fetch(&request).unwrap_err();
        assert!(error.to_string().contains("offline mode"));
    }

    #[test]
    fn parallel_fetch_deduplicates_identical_digests() {
        let temp = tempfile::tempdir().unwrap();
        let (_home, manager) = manager(&temp);
        let source = temp.path().join("shared.jar");
        fs::write(&source, b"shared").unwrap();
        let digest = fsutil::sha1_bytes(b"shared");
        let url = format!("file://{}", source.display());
        let requests = vec![
            ResourceRequest::new("a", ResourceKind::Library, url.clone())
                .with_sha1(Some(digest.clone())),
            ResourceRequest::new("b", ResourceKind::Library, url.clone())
                .with_sha1(Some(digest.clone())),
            ResourceRequest::new("c", ResourceKind::Library, url).with_sha1(Some(digest.clone())),
        ];
        let fetched = manager.fetch_many(&requests).unwrap();
        assert_eq!(fetched.len(), 3);
        assert_eq!(fetched[0].path, fetched[2].path);
        assert_eq!(fetched.iter().filter(|f| !f.from_cache).count(), 1);
    }

    #[test]
    fn reports_every_failure_in_a_batch() {
        let temp = tempfile::tempdir().unwrap();
        let (_home, manager) = manager(&temp);
        let requests = vec![
            ResourceRequest::new("a", ResourceKind::Library, "https://example.invalid/a.jar"),
            ResourceRequest::new("b", ResourceKind::Library, "https://example.invalid/b.jar"),
        ];
        let error = manager.fetch_many(&requests).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("2 of 2"));
    }

    #[test]
    fn materialize_links_or_copies() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.jar");
        fs::write(&source, b"data").unwrap();
        let target = temp.path().join("libs/source.jar");
        materialize(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"data");
        // Second call is a no-op.
        materialize(&source, &target).unwrap();
        assert!(target.is_file());
    }

    #[test]
    fn progress_reporter_receives_events() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let config = Config::default();
        let events: Arc<Mutex<Vec<ProgressEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let manager = ResourceManager::new(&home, &config)
            .unwrap()
            .with_reporter(Arc::new(move |update: &ProgressUpdate| {
                sink.lock().unwrap().push(update.event.clone());
            }));
        let source = temp.path().join("x.jar");
        let mut file = File::create(&source).unwrap();
        file.write_all(&vec![7u8; 600 * 1024]).unwrap();
        drop(file);
        let request = ResourceRequest::new(
            "x",
            ResourceKind::Library,
            format!("file://{}", source.display()),
        );
        manager.fetch(&request).unwrap();
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProgressEvent::Finished { .. }))
        );
    }

    #[test]
    fn metadata_cache_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let (_home, manager) = manager(&temp);
        let source = temp.path().join("meta.json");
        fs::write(&source, br#"{"versions":[{"id":"26.3"}]}"#).unwrap();
        let url = format!("file://{}", source.display());
        let value = manager
            .fetch_json(&url, "manifest", Duration::from_secs(3600))
            .unwrap();
        assert_eq!(value["versions"][0]["id"], "26.3");
        // Offline + cache still serves the value.
        let value = manager
            .fetch_json(&url, "manifest", Duration::from_secs(0))
            .unwrap();
        assert_eq!(value["versions"][0]["id"], "26.3");
    }
}
