//! Official Minecraft resource management for MATS.
//!
//! * content-addressed cache with SHA-1 verification
//! * resumable, parallel downloads with bounded retries
//! * offline operation against a warm cache
//! * safe extraction of natives and Java runtimes
//!
//! Only resources required by the selected test environment are fetched; MATS
//! never mirrors a full Minecraft installation.

pub mod cache;
pub mod download;
pub mod extract;
pub mod http;

pub use cache::{CacheStats, ObjectCache};
pub use download::{
    FetchedResource, ProgressEvent, ProgressFn, ProgressUpdate, ResourceManager, ResourceRequest,
    materialize, stderr_reporter,
};
pub use extract::{
    extract_tar_gz_flattened, extract_tar_gz_safely, extract_zip_flattened, extract_zip_safely,
    read_zip_entry,
};
pub use http::{HttpClient, NetError, encode_segment, local_path_from_url};
