//! Self-update support.
//!
//! MATS only ever downloads release assets from the configured GitHub
//! repository, verifies the SHA-256 from the release's checksum manifest and
//! refuses to replace a working executable when anything is missing or does
//! not match. On Windows the replacement happens from a temporary helper
//! process after the running binary exits; elsewhere the executable is
//! replaced atomically with a backup that is restored on failure.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mats_core::config::{Config, UpdateChannel};
use mats_core::types::ResourceKind;
use mats_core::{MatsHome, fsutil};
use resource_manager::{ResourceManager, ResourceRequest};
use semver::Version;
use serde::{Deserialize, Serialize};

/// How often the automatic check may run, at most.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_STATE_FILE: &str = "update-check.json";
const ASSET_PREFIX: &str = "mats";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub size: u64,
    /// GitHub's per-asset digest (`sha256:...`), when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStatus {
    /// The installed version is the newest available.
    UpToDate,
    /// A newer stable release exists.
    Available,
    /// A newer prerelease exists (only when the prerelease channel is used).
    PrereleaseAvailable,
    /// No release provides an executable for this platform.
    UnsupportedPlatform,
    /// The check could not run.
    Failed,
    /// Updates are disabled in configuration.
    Disabled,
}

impl UpdateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateStatus::UpToDate => "up-to-date",
            UpdateStatus::Available => "update-available",
            UpdateStatus::PrereleaseAvailable => "prerelease-available",
            UpdateStatus::UnsupportedPlatform => "unsupported-platform",
            UpdateStatus::Failed => "failed",
            UpdateStatus::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheck {
    pub current_version: String,
    pub channel: UpdateChannel,
    pub status: UpdateStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<ReleaseInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<ReleaseAsset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl UpdateCheck {
    pub fn is_update_available(&self) -> bool {
        matches!(
            self.status,
            UpdateStatus::Available | UpdateStatus::PrereleaseAvailable
        )
    }
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub installed: bool,
    pub version: String,
    pub backup: Option<PathBuf>,
    pub message: String,
    pub restart_required: bool,
}

/// Current MATS version, taken from the build metadata.
pub fn current_version() -> Result<Version> {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .with_context(|| format!("invalid built-in version `{}`", env!("CARGO_PKG_VERSION")))
}

/// `(operating system, architecture)` slugs used in release asset names.
pub fn platform_slug() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "x86_64"
    };
    (os, arch)
}

/// Query GitHub for the newest release on the configured channel.
pub fn check(manager: &ResourceManager, config: &Config) -> Result<UpdateCheck> {
    let current = current_version()?;
    let channel = config.updates.channel;
    if !config.updates.enabled {
        return Ok(UpdateCheck {
            current_version: current.to_string(),
            channel,
            status: UpdateStatus::Disabled,
            latest: None,
            asset: None,
            checksum_source: None,
            notes: vec!["update checks are disabled in config.toml".to_string()],
        });
    }
    if manager.offline() {
        bail!("offline mode is enabled; update checks need network access");
    }
    let repository = config.updates.repository.trim();
    if !repository.contains('/') {
        bail!("updates.repository must be `owner/repo`");
    }
    let api = config.network.github_api.trim_end_matches('/');
    let url = if channel.include_prereleases() {
        format!("{api}/repos/{repository}/releases?per_page=30")
    } else {
        format!("{api}/repos/{repository}/releases/latest")
    };
    let value = fetch_github_json(manager, &url)?;
    let releases = normalize_releases(value, channel)?;
    let Some(latest) = releases.into_iter().max_by(|left, right| {
        version_of(left)
            .unwrap_or_else(|_| Version::new(0, 0, 0))
            .cmp(&version_of(right).unwrap_or_else(|_| Version::new(0, 0, 0)))
    }) else {
        bail!("the repository has no published releases");
    };
    let latest_version = version_of(&latest)?;
    let (os, arch) = platform_slug();
    let asset = asset_for_platform(&latest.assets, os, arch);
    let mut notes = Vec::new();
    if let Some(asset) = &asset {
        notes.push(format!(
            "release asset: {} ({})",
            asset.name,
            fsutil::human_bytes(asset.size)
        ));
    }
    let status = if latest_version <= current {
        UpdateStatus::UpToDate
    } else if asset.is_none() {
        UpdateStatus::UnsupportedPlatform
    } else if latest.prerelease {
        UpdateStatus::PrereleaseAvailable
    } else {
        UpdateStatus::Available
    };
    if latest_version > current && asset.is_none() {
        notes.push(format!(
            "release {} does not publish an executable for {os}-{arch}; available assets: {}",
            latest.tag,
            latest
                .assets
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let checksum_source = checksum_asset(&latest.assets).map(|asset| asset.name.clone());
    Ok(UpdateCheck {
        current_version: current.to_string(),
        channel,
        status,
        latest: Some(latest),
        asset,
        checksum_source,
        notes,
    })
}

fn fetch_github_json(manager: &ResourceManager, url: &str) -> Result<serde_json::Value> {
    match manager.fetch_json_uncached(url) {
        Ok(value) => Ok(value),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("403") || message.contains("429") {
                bail!(
                    "GitHub API request failed ({url}). This is usually the unauthenticated rate limit; retry later or download the release manually."
                )
            }
            Err(error)
        }
    }
}

fn normalize_releases(
    value: serde_json::Value,
    channel: UpdateChannel,
) -> Result<Vec<ReleaseInfo>> {
    let array = match value {
        serde_json::Value::Array(array) => array,
        serde_json::Value::Object(_) => vec![value],
        _ => bail!("GitHub returned an unexpected release payload"),
    };
    let mut releases = Vec::new();
    for entry in array {
        let draft = entry
            .get("draft")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if draft {
            continue;
        }
        let prerelease = entry
            .get("prerelease")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if prerelease && !channel.include_prereleases() {
            continue;
        }
        let tag = entry
            .get("tag_name")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        if tag.is_empty() {
            continue;
        }
        let assets = entry
            .get("assets")
            .and_then(|value| value.as_array())
            .map(|assets| {
                assets
                    .iter()
                    .filter_map(|asset| {
                        Some(ReleaseAsset {
                            name: asset.get("name")?.as_str()?.to_string(),
                            url: asset
                                .get("browser_download_url")
                                .and_then(|value| value.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            size: asset
                                .get("size")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0),
                            digest: asset
                                .get("digest")
                                .and_then(|value| value.as_str())
                                .map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        releases.push(ReleaseInfo {
            version: tag.trim_start_matches('v').to_string(),
            tag,
            name: entry
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            body: entry
                .get("body")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            published_at: entry
                .get("published_at")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            prerelease,
            html_url: entry
                .get("html_url")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            assets,
        });
    }
    Ok(releases)
}

fn version_of(release: &ReleaseInfo) -> Result<Version> {
    Version::parse(&release.version).with_context(|| {
        format!(
            "release tag `{}` is not a semantic version; MATS cannot compare it",
            release.tag
        )
    })
}

/// Pick the executable matching this platform.
pub fn asset_for_platform(assets: &[ReleaseAsset], os: &str, arch: &str) -> Option<ReleaseAsset> {
    let suffix = format!("-{os}-{arch}");
    let ext = if os == "windows" { ".exe" } else { "" };
    let expected_suffix = format!("{suffix}{ext}");
    assets
        .iter()
        .find(|asset| {
            asset.name.starts_with(ASSET_PREFIX)
                && asset.name.ends_with(&expected_suffix)
                && !asset.name.contains("SHA256")
        })
        .cloned()
        .or_else(|| {
            // Tolerate assets without the `.exe` suffix in the manifest.
            assets
                .iter()
                .find(|asset| {
                    asset.name.starts_with(ASSET_PREFIX)
                        && asset.name.ends_with(&suffix)
                        && !asset.name.contains("SHA256")
                })
                .cloned()
        })
}

fn checksum_asset(assets: &[ReleaseAsset]) -> Option<&ReleaseAsset> {
    assets.iter().find(|asset| {
        let upper = asset.name.to_ascii_uppercase();
        upper.contains("SHA256") && !upper.ends_with(".EXE")
    })
}

/// Parse a `SHA256SUMS` manifest (`<hex>  <filename>` per line).
pub fn parse_sha256_sums(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Some(name) = parts.next() else {
            continue;
        };
        map.insert(
            name.trim_start_matches('*').trim().to_string(),
            hash.to_ascii_lowercase(),
        );
    }
    map
}

/// Information collected for installation.
#[derive(Debug, Clone)]
pub struct PreparedUpdate {
    pub version: Version,
    pub file: PathBuf,
    pub sha256: String,
    pub asset_name: String,
}

/// Download and verify the release asset for an available update.
pub fn download_verified(
    manager: &ResourceManager,
    check: &UpdateCheck,
    home: &MatsHome,
    allow_downgrade: bool,
) -> Result<PreparedUpdate> {
    let release = check
        .latest
        .as_ref()
        .context("no release information is available")?;
    let asset = check
        .asset
        .as_ref()
        .context("no executable is available for this platform")?;
    let version = version_of(release)?;
    let current = current_version()?;
    if version <= current && !allow_downgrade {
        bail!(
            "release {} is not newer than the installed version {current}; refusing to downgrade (pass --allow-downgrade to override)",
            version
        );
    }

    let expected = if let Some(digest) = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
    {
        Some(digest.to_ascii_lowercase())
    } else if let Some(manifest) = checksum_asset(&release.assets) {
        let text = manager
            .fetch_text(
                &manifest.url,
                &format!("update-checksums-{}", release.tag),
                Duration::from_secs(60 * 10),
            )
            .with_context(|| format!("downloading {}", manifest.name))?;
        let sums = parse_sha256_sums(&text);
        sums.get(&asset.name).cloned()
    } else {
        None
    };
    let Some(expected) = expected else {
        bail!(
            "release {} does not publish a SHA-256 checksum for {}; refusing to install an unverified update",
            release.tag,
            asset.name
        );
    };

    let downloads = home.tmp_dir().join("updates");
    fs::create_dir_all(&downloads).with_context(|| format!("creating {}", downloads.display()))?;
    let request = ResourceRequest::new(
        format!("update-{}", release.tag),
        ResourceKind::Loader,
        asset.url.clone(),
    )
    .with_size(if asset.size > 0 {
        Some(asset.size)
    } else {
        None
    });
    let fetched = manager
        .fetch(&request)
        .with_context(|| format!("downloading {} from {}", asset.name, asset.url))?;
    let actual = fsutil::sha256_file(&fetched.path)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = fs::remove_file(&fetched.path);
        bail!(
            "SHA-256 verification failed for {}: expected {expected}, downloaded content hashes to {actual}. The update was not installed.",
            asset.name
        );
    }
    let staged = downloads.join(format!("{}-{}", release.tag, asset.name));
    fs::copy(&fetched.path, &staged).with_context(|| {
        format!(
            "copying verified update {} to {}",
            fetched.path.display(),
            staged.display()
        )
    })?;
    Ok(PreparedUpdate {
        version,
        file: staged,
        sha256: actual,
        asset_name: asset.name.clone(),
    })
}

/// Install a verified update over `target`.
pub fn install(
    manager: &ResourceManager,
    check: &UpdateCheck,
    home: &MatsHome,
    target: &Path,
    allow_downgrade: bool,
    restart: bool,
) -> Result<InstallOutcome> {
    if !target.is_file() {
        bail!(
            "cannot update `{}`: the executable does not exist",
            target.display()
        );
    }
    let prepared = download_verified(manager, check, home, allow_downgrade)?;
    let size = fs::metadata(&prepared.file)?.len();
    if size == 0 {
        bail!("the downloaded update is empty; refusing to install it");
    }

    #[cfg(windows)]
    {
        let helper = spawn_windows_helper(&prepared.file, target, restart)?;
        return Ok(InstallOutcome {
            installed: true,
            version: prepared.version.to_string(),
            backup: None,
            message: format!(
                "MATS {} will be installed by {} after the current process exits. Restart MATS to use it.",
                prepared.version,
                helper.display()
            ),
            restart_required: true,
        });
    }
    #[cfg(not(windows))]
    {
        let backup = replace_unix(&prepared.file, target)?;
        Ok(InstallOutcome {
            installed: true,
            version: prepared.version.to_string(),
            backup: Some(backup),
            message: format!(
                "MATS {} installed. The previous executable was kept as a backup.",
                prepared.version
            ),
            restart_required: restart,
        })
    }
}

#[cfg(not(windows))]
fn replace_unix(new_file: &Path, target: &Path) -> Result<PathBuf> {
    let parent = target
        .parent()
        .context("the executable path has no parent directory")?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("the executable path has no file name")?;
    let staged = parent.join(format!(".{file_name}.new-{}", std::process::id()));
    fs::copy(new_file, &staged)
        .with_context(|| format!("staging the update at {}", staged.display()))?;
    fsutil::set_executable(&staged)?;
    let backup = parent.join(format!(".{file_name}.old-{}", std::process::id()));
    let _ = fs::remove_file(&backup);
    fs::rename(target, &backup).with_context(|| {
        format!(
            "backing up {} to {} (do you have permission to write there?)",
            target.display(),
            backup.display()
        )
    })?;
    if let Err(error) = fs::rename(&staged, target) {
        let _ = fs::rename(&backup, target);
        let _ = fs::remove_file(&staged);
        return Err(error).with_context(|| {
            format!(
                "replacing {} failed; the previous executable was restored",
                target.display()
            )
        });
    }
    let _ = fs::remove_file(new_file);
    Ok(backup)
}

#[cfg(windows)]
fn spawn_windows_helper(new_file: &Path, target: &Path, restart: bool) -> Result<PathBuf> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    let temp = std::env::temp_dir();
    let helper = temp.join(format!("mats-updater-{}.exe", std::process::id()));
    fs::copy(std::env::current_exe()?, &helper)
        .with_context(|| format!("creating the update helper at {}", helper.display()))?;
    let mut command = Command::new(&helper);
    command.args([
        "--mats-apply-update",
        "--wait-pid",
        &std::process::id().to_string(),
        "--source",
        &new_file.display().to_string(),
        "--target",
        &target.display().to_string(),
    ]);
    if restart {
        command.arg("--restart");
    }
    command.creation_flags(0x0800_0000);
    command
        .spawn()
        .with_context(|| "starting the Windows update helper")?;
    Ok(helper)
}

/// Entry point for the detached Windows update helper.
///
/// On non-Windows platforms this is still implemented (using the same safe
/// replacement logic) so the behaviour can be tested everywhere.
pub fn apply_update_helper(
    wait_pid: u32,
    source: &Path,
    target: &Path,
    restart: bool,
) -> Result<()> {
    wait_for_process_exit(wait_pid, Duration::from_secs(120));
    let mut last_error = None;
    for attempt in 0..30 {
        match replace_for_helper(source, target) {
            Ok(()) => {
                if restart {
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        let _ = std::process::Command::new(target)
                            .creation_flags(0x0000_0008)
                            .spawn();
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = std::process::Command::new(target).spawn();
                    }
                }
                return Ok(());
            }
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(500 * (attempt + 1)));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("the update could not be applied")))
}

fn replace_for_helper(source: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().context("target has no parent directory")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("target has no file name")?;
    let backup = parent.join(format!("{name}.old"));
    let _ = fs::remove_file(&backup);
    if target.exists() {
        fs::rename(target, &backup)
            .with_context(|| format!("moving the current executable {} aside", target.display()))?;
    }
    match fs::copy(source, target) {
        Ok(_) => {
            fsutil::set_executable(target)?;
            let _ = fs::remove_file(source);
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(error) => {
            if backup.exists() {
                let _ = fs::rename(&backup, target);
            }
            Err(error).with_context(|| {
                format!(
                    "replacing {} failed; the previous executable was restored",
                    target.display()
                )
            })
        }
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !process_is_running(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    tracing::warn!(pid, "timed out waiting for the MATS process to exit");
}

#[cfg(target_os = "linux")]
fn process_is_running(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(target_os = "macos")]
fn process_is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match output {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_is_running(_pid: u32) -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CheckState {
    last_checked: i64,
    last_version: String,
}

fn state_path(home: &MatsHome) -> PathBuf {
    home.meta_dir().join(CHECK_STATE_FILE)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// `true` when the automatic check interval has elapsed.
pub fn should_check(home: &MatsHome) -> bool {
    let Ok(text) = fs::read_to_string(state_path(home)) else {
        return true;
    };
    let Ok(state) = serde_json::from_str::<CheckState>(&text) else {
        return true;
    };
    now_unix().saturating_sub(state.last_checked) >= CHECK_INTERVAL.as_secs() as i64
}

/// Record that a check ran, so automatic checks stay throttled.
pub fn record_check(home: &MatsHome, version: &str) {
    let state = CheckState {
        last_checked: now_unix(),
        last_version: version.to_string(),
    };
    let _ = fs::create_dir_all(home.meta_dir());
    if let Ok(text) = serde_json::to_vec(&state) {
        let _ = fsutil::atomic_write(&state_path(home), &text);
    }
}

/// Render a short human-readable update notice.
pub fn render_check(check: &UpdateCheck, verbose: bool) -> String {
    let mut out = String::new();
    match check.status {
        UpdateStatus::UpToDate => {
            out.push_str(&format!(
                "MATS {} is up to date on the {} channel.\n",
                check.current_version,
                check.channel.as_str()
            ));
        }
        UpdateStatus::Available | UpdateStatus::PrereleaseAvailable => {
            let release = check.latest.as_ref();
            out.push_str(&format!(
                "Update available: MATS {} → {}{}\n",
                check.current_version,
                release
                    .map(|release| release.version.clone())
                    .unwrap_or_default(),
                if check.status == UpdateStatus::PrereleaseAvailable {
                    " (prerelease)"
                } else {
                    ""
                }
            ));
            if let Some(release) = release {
                if !release.published_at.is_empty() {
                    out.push_str(&format!("Released: {}\n", release.published_at));
                }
                if let Some(asset) = &check.asset {
                    out.push_str(&format!(
                        "Asset: {} ({})\n",
                        asset.name,
                        mats_core::fsutil::human_bytes(asset.size)
                    ));
                }
                if let Some(source) = &check.checksum_source {
                    out.push_str(&format!("Checksum: {source}\n"));
                }
                if verbose && !release.body.is_empty() {
                    out.push_str("\nRelease notes:\n");
                    for line in release.body.lines().take(40) {
                        out.push_str(&format!("  {line}\n"));
                    }
                }
            }
            out.push_str("Run `mats update install` to install it.\n");
        }
        UpdateStatus::UnsupportedPlatform => {
            out.push_str(&format!(
                "MATS {} is installed, but the latest release does not provide a build for this platform.\n",
                check.current_version
            ));
        }
        UpdateStatus::Disabled => {
            out.push_str("Update checks are disabled in config.toml.\n");
        }
        UpdateStatus::Failed => {
            out.push_str("The update check failed.\n");
        }
    }
    for note in &check.notes {
        out.push_str(&format!("  - {note}\n"));
    }
    out
}

/// Write the downloaded bytes for tests and tooling.
pub fn write_download(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            url: format!("https://example.invalid/{name}"),
            size: 100,
            digest: None,
        }
    }

    #[test]
    fn selects_the_current_platform_asset() {
        // Keep fixtures for every release target so this test is portable.
        let assets = vec![
            asset("mats-v1.1.0-windows-x86_64.exe"),
            asset("mats-v1.1.0-windows-aarch64.exe"),
            asset("mats-v1.1.0-linux-x86_64"),
            asset("mats-v1.1.0-linux-aarch64"),
            asset("mats-v1.1.0-macos-x86_64"),
            asset("mats-v1.1.0-macos-aarch64"),
            asset("SHA256SUMS"),
        ];
        let (os, arch) = platform_slug();
        let expected = format!("mats-v1.1.0-{os}-{arch}");
        let selected = asset_for_platform(&assets, os, arch).unwrap();
        assert!(selected.name.starts_with(&expected));
        assert!(asset_for_platform(&assets, "plan9", "vax").is_none());
    }

    #[test]
    fn parses_checksum_manifests() {
        let hash = "a".repeat(64);
        let text = format!(
            "{hash}  mats-v1.1.0-linux-x86_64\n{hash} *mats-v1.1.0-windows-x86_64.exe\nbogus line\n"
        );
        let sums = parse_sha256_sums(&text);
        assert_eq!(sums.get("mats-v1.1.0-linux-x86_64").unwrap(), &hash);
        assert_eq!(sums.get("mats-v1.1.0-windows-x86_64.exe").unwrap(), &hash);
        assert_eq!(sums.len(), 2);
    }

    #[test]
    fn platform_slugs_are_supported() {
        let (os, arch) = platform_slug();
        assert!(["windows", "linux", "macos"].contains(&os));
        assert!(["x86_64", "aarch64", "arm"].contains(&arch));
    }

    #[test]
    fn semantic_version_comparison_detects_downgrades() {
        let current = Version::parse("1.0.0").unwrap();
        assert!(Version::parse("1.0.1").unwrap() > current);
        assert!(Version::parse("0.9.9").unwrap() < current);
    }

    #[test]
    fn helper_replaces_and_restores_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("mats");
        fs::write(&target, b"old version").unwrap();
        let source = temp.path().join("mats.new");
        fs::write(&source, b"new version").unwrap();
        apply_update_helper(std::process::id(), &source, &target, false).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new version");

        // A missing source must leave the target intact.
        let missing = temp.path().join("absent");
        let result = replace_for_helper(&missing, &target);
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new version");
    }

    #[test]
    fn check_state_throttles_automatic_checks() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        home.ensure_layout().unwrap();
        assert!(should_check(&home));
        record_check(&home, "1.0.0");
        assert!(!should_check(&home));
    }

    #[test]
    fn renders_available_and_current_states() {
        let check = UpdateCheck {
            current_version: "1.0.0".into(),
            channel: UpdateChannel::Stable,
            status: UpdateStatus::Available,
            latest: Some(ReleaseInfo {
                tag: "v1.1.0".into(),
                version: "1.1.0".into(),
                name: "MATS 1.1.0".into(),
                body: "Fixes".into(),
                published_at: "2026-11-01T00:00:00Z".into(),
                prerelease: false,
                html_url: String::new(),
                assets: vec![asset("mats-v1.1.0-linux-x86_64")],
            }),
            asset: Some(asset("mats-v1.1.0-linux-x86_64")),
            checksum_source: Some("SHA256SUMS".into()),
            notes: Vec::new(),
        };
        let text = render_check(&check, true);
        assert!(text.contains("1.0.0 → 1.1.0"));
        assert!(text.contains("mats update install"));
        assert!(text.contains("Fixes"));

        let up_to_date = UpdateCheck {
            status: UpdateStatus::UpToDate,
            ..check
        };
        assert!(render_check(&up_to_date, false).contains("up to date"));
    }

    #[test]
    fn disabled_updates_short_circuit() {
        let temp = tempfile::tempdir().unwrap();
        let home = MatsHome::new(temp.path().join("home"));
        let mut config = Config::default();
        config.updates.enabled = false;
        let manager = ResourceManager::new(&home, &config).unwrap();
        let check = check(&manager, &config).unwrap();
        assert_eq!(check.status, UpdateStatus::Disabled);
    }
}
