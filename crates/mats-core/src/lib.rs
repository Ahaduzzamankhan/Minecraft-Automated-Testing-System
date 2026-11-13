//! Shared domain model, configuration and cache layout for MATS.
//!
//! Everything in this crate is loader-agnostic and side-effect free with the
//! exception of [`paths`], [`config`] and [`fsutil`], which touch the
//! filesystem.

pub mod config;
pub mod fsutil;
pub mod paths;
pub mod types;
pub mod version;

pub use config::{Config, DisplayMode, UpdateChannel};
pub use paths::MatsHome;
pub use types::{
    DependencySpec, Finding, FindingCategory, JarFingerprint, Loader, Metrics, ModMetadata, Report,
    RunSummary, Severity, Side, TestOutcome, TestStatus,
};
pub use version::{McVersion, VersionPredicate};
