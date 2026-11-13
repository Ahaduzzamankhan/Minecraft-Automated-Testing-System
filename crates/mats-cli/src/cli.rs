//! Command line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "mats",
    version,
    about = "MATS — Minecraft Automated Testing System",
    long_about = "MATS inspects Minecraft mod JARs, resolves the loader and Minecraft version they \
                  target, downloads the official Minecraft resources needed, runs the real game in an \
                  isolated JVM and produces diagnostic reports.\n\n\
                  Mod JARs are inspected without execution. Runtime tests only start when you run \
                  `mats test`.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Override the MATS home directory (config, cache, environments, reports).
    #[arg(long, global = true, env = "MATS_HOME", value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// Load configuration from a specific file.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Print additional diagnostic logging.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Only print essential output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Emit machine-readable JSON where supported.
    #[arg(long, global = true)]
    pub json: bool,

    /// Skip the automatic update check for this invocation.
    #[arg(long, global = true)]
    pub no_update_check: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect a mod JAR, test it in a real Minecraft runtime and write a report.
    Test(TestArgs),
    /// Show the mod metadata MATS found without executing anything.
    Inspect {
        /// Path to the mod JAR.
        jar: PathBuf,
    },
    /// Deep static analysis: metadata, mixins, bytecode and conflicts.
    Analyze {
        /// Path to the mod JAR.
        jar: PathBuf,
        /// Additional mods to compare against for conflicts.
        #[arg(long = "with", value_name = "JAR")]
        extra_mods: Vec<PathBuf>,
    },
    /// Show which Minecraft versions and loader builds can be tested.
    Matrix {
        /// Path to the mod JAR.
        jar: PathBuf,
        /// Restrict to a specific Minecraft version.
        #[arg(long)]
        mc: Option<String>,
        /// Loader to plan for (defaults to the loader detected in the JAR).
        #[arg(long)]
        loader: Option<String>,
        /// Maximum number of candidate versions.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Include snapshots when no release matches.
        #[arg(long = "allow-snapshots")]
        allow_snapshots: bool,
    },
    /// List recent Minecraft releases and available loader builds.
    Versions {
        /// Also resolve the newest loader build for the newest release.
        #[arg(long)]
        loader: Option<String>,
        /// How many releases to list.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Diagnose the local environment: network, Java, cache, graphics.
    Doctor,
    /// Inspect or clean the managed resource cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Inspect or clean prepared test environments.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Show recent test runs.
    History {
        /// How many runs to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print the most recent report.
    Report {
        /// Print the report directory instead of its contents.
        #[arg(long)]
        path: bool,
    },
    /// Print the MATS version.
    Version,
    /// Check for or install a MATS update.
    Update(UpdateArgs),
}

#[derive(Debug, Args)]
pub struct TestArgs {
    /// Path to the mod JAR to test.
    pub jar: PathBuf,

    /// Test a specific Minecraft version instead of resolving it from metadata.
    #[arg(long, value_name = "VERSION")]
    pub mc: Option<String>,

    /// Force a loader (fabric, quilt, forge, neoforge, vanilla).
    #[arg(long, value_name = "LOADER")]
    pub loader: Option<String>,

    /// Pin a specific loader build.
    #[arg(long = "loader-version", value_name = "VERSION")]
    pub loader_version: Option<String>,

    /// Which side to test: client, server or both.
    #[arg(long, value_name = "SIDE")]
    pub side: Option<String>,

    /// Per-run timeout in seconds.
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    /// Maximum JVM heap in MiB.
    #[arg(long, value_name = "MIB")]
    pub memory: Option<u32>,

    /// Install an additional mod alongside the tested one.
    #[arg(long = "with", value_name = "JAR")]
    pub extra_mods: Vec<PathBuf>,

    /// Accept the Minecraft EULA (required for dedicated server tests).
    #[arg(long = "accept-eula")]
    pub accept_eula: bool,

    /// Use only the local cache; never touch the network.
    #[arg(long)]
    pub offline: bool,

    /// Rebuild the environment even if a matching one is cached.
    #[arg(long)]
    pub fresh: bool,

    /// Download the full client asset object set.
    #[arg(long = "download-assets")]
    pub download_assets: bool,

    /// Do not download required mod dependencies automatically.
    #[arg(long = "no-deps")]
    pub no_deps: bool,

    /// Client display strategy: auto, native, virtual (Xvfb) or disabled.
    #[arg(long, value_name = "MODE")]
    pub display: Option<String>,

    /// Allow snapshot versions when no stable release matches.
    #[arg(long = "allow-snapshots")]
    pub allow_snapshots: bool,

    /// Write reports to a specific directory.
    #[arg(long = "report-dir", value_name = "DIR")]
    pub report_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Show cache size, location and contents.
    Info,
    /// Remove cached Minecraft resources and metadata.
    Clean {
        /// Also remove prepared environments.
        #[arg(long)]
        envs: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum EnvAction {
    /// List prepared environments.
    List,
    /// Remove prepared environments.
    Clean {
        /// Remove every environment.
        #[arg(long)]
        all: bool,
        /// Only remove environments older than this many hours.
        #[arg(long = "older-than-hours", value_name = "HOURS")]
        older_than_hours: Option<u64>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub action: Option<UpdateAction>,

    /// Update channel to use for this invocation.
    #[arg(long, value_name = "CHANNEL")]
    pub channel: Option<String>,

    /// Install without asking for confirmation.
    #[arg(long)]
    pub yes: bool,

    /// Allow installing an older version than the one currently running.
    #[arg(long = "allow-downgrade")]
    pub allow_downgrade: bool,

    /// Show the installed and latest available versions only.
    #[arg(long = "version")]
    pub show_version: bool,
}

#[derive(Debug, Subcommand)]
pub enum UpdateAction {
    /// Check whether a newer release exists.
    Check,
    /// Download, verify and install the newest release.
    Install,
}
