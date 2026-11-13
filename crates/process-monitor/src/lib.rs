//! Process supervision for real JVM test runs.
//!
//! Every run happens in its own process with piped output, a hard timeout and
//! optional startup-marker based shutdown. Standard output and standard error
//! are captured to separate files plus a combined, prefixed log.

pub mod launch;
pub mod sampler;

pub use launch::{LaunchSpec, ProcessOutcome, launch};
pub use sampler::{Peaks, SAMPLE_INTERVAL, Sampler};
