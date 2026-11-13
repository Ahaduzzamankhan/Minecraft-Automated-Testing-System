//! Resolution of Minecraft, mod loader and Java versions.

pub mod loaders;
pub mod mojang;
pub mod plan;

pub use loaders::{InstallerInfo, LoaderArtifact, LoaderResolver, maven_versions};
pub use mojang::{
    AssetIndex, DownloadInfo, JavaVersion, Library, ManifestEntry, MojangClient, MojangVersion,
    Rule, VersionManifest, fallback_java_major,
};
pub use plan::{
    CandidateSelection, MatrixRow, PlanRequest, ResolvedPlan, build_matrix, resolve_plan,
    select_candidates,
};
