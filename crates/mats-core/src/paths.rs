//! Filesystem layout of the managed MATS home directory.
//!
//! The home directory holds the configuration, the content-addressed resource
//! cache, downloaded JVMs, prepared test environments, run history and
//! generated reports. It is deliberately outside the executable so that the
//! binary stays small and Minecraft data stays inspectable.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Environment variable that redirects the whole MATS home (used by tests).
pub const HOME_ENV: &str = "MATS_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatsHome {
    root: PathBuf,
}

impl MatsHome {
    pub fn discover() -> Result<Self> {
        if let Some(raw) = env::var_os(HOME_ENV) {
            let path = PathBuf::from(raw);
            if path.as_os_str().is_empty() {
                anyhow::bail!("{HOME_ENV} is set but empty");
            }
            return Ok(MatsHome { root: path });
        }
        let base = dirs::data_local_dir()
            .or_else(dirs::home_dir)
            .context("could not determine a platform data directory for the MATS home")?;
        Ok(MatsHome {
            root: base.join("mats"),
        })
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        MatsHome { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Content-addressed object store keyed by SHA-1/SHA-256 digests.
    pub fn objects_dir(&self) -> PathBuf {
        self.cache_dir().join("objects")
    }

    /// Cached upstream metadata (Mojang manifests, loader APIs).
    pub fn meta_dir(&self) -> PathBuf {
        self.cache_dir().join("meta")
    }

    pub fn java_dir(&self) -> PathBuf {
        self.cache_dir().join("java")
    }

    /// Prepared, isolated runtime environments.
    pub fn envs_dir(&self) -> PathBuf {
        self.root.join("envs")
    }

    pub fn reports_dir(&self) -> PathBuf {
        self.root.join("reports")
    }

    pub fn history_file(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.cache_dir(),
            self.objects_dir(),
            self.meta_dir(),
            self.java_dir(),
            self.envs_dir(),
            self.reports_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating MATS directory {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_relative_to_root() {
        let home = MatsHome::new("/tmp/mats-test");
        assert_eq!(home.config_file(), Path::new("/tmp/mats-test/config.toml"));
        assert_eq!(
            home.objects_dir(),
            Path::new("/tmp/mats-test/cache/objects")
        );
        assert_eq!(home.envs_dir(), Path::new("/tmp/mats-test/envs"));
        assert_eq!(
            home.history_file(),
            Path::new("/tmp/mats-test/history.jsonl")
        );
    }

    #[test]
    fn ensure_layout_creates_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = MatsHome::new(temp.path().join("mats"));
        home.ensure_layout().expect("layout");
        assert!(home.objects_dir().is_dir());
        assert!(home.envs_dir().is_dir());
        assert!(home.reports_dir().is_dir());
    }
}
