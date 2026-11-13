//! Small, dependency-light filesystem helpers with a focus on safety.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Hash a file with SHA-1 (Mojang metadata uses SHA-1 digests).
pub fn sha1_file(path: &Path) -> Result<String> {
    let reader =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut digest = Sha1::new();
    hash_reader(reader, &mut digest)?;
    Ok(hex::encode(digest.finalize()))
}

/// Hash a file with SHA-256 (release artifacts and update verification).
pub fn sha256_file(path: &Path) -> Result<String> {
    let reader =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut digest = Sha256::new();
    hash_reader(reader, &mut digest)?;
    Ok(hex::encode(digest.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

pub fn sha1_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha1::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn hash_reader<R: Read, D: Digest>(mut reader: R, digest: &mut D) -> Result<()> {
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("reading file for hashing")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(())
}

/// Write a file via a sibling temp file + rename so readers never observe a
/// partially written file.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let temp_path = path.with_extension(format!(
        "tmp-{}",
        std::process::id() as u64 ^ timestamp_nanos()
    ));
    {
        let mut file = File::create(&temp_path)
            .with_context(|| format!("creating {}", temp_path.display()))?;
        file.write_all(contents)
            .with_context(|| format!("writing {}", temp_path.display()))?;
        file.sync_all().ok();
    }
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "moving {} into place at {}",
            temp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn timestamp_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

/// Copy a file through a temporary sibling and an atomic rename.
pub fn atomic_copy(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let temp_path = target.with_extension(format!(
        "copy-{}",
        std::process::id() as u64 ^ timestamp_nanos()
    ));
    fs::copy(source, &temp_path)
        .with_context(|| format!("copying {} to {}", source.display(), temp_path.display()))?;
    fs::rename(&temp_path, target)
        .with_context(|| format!("moving {} into place", target.display()))?;
    Ok(())
}

/// Resolve `relative` under `base` while rejecting absolute paths, parent
/// traversal, prefixes and (on Windows) alternate data streams.
pub fn safe_join(base: &Path, relative: &str) -> Result<PathBuf> {
    let candidate = Path::new(relative);
    validate_relative(candidate)?;
    Ok(base.join(candidate))
}

/// Validate that a path is relative and free of traversal components.
pub fn validate_relative(path: &Path) -> Result<()> {
    if path.is_absolute() {
        anyhow::bail!("absolute path `{}` is not allowed", path.display());
    }
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_string_lossy();
                if text == ".." || text == "." || text.is_empty() {
                    anyhow::bail!("path `{}` contains an invalid component", path.display());
                }
                if text.contains(':') {
                    anyhow::bail!(
                        "path `{}` contains a drive/stream separator",
                        path.display()
                    );
                }
            }
            Component::CurDir => {}
            _ => anyhow::bail!("path `{}` is not a safe relative path", path.display()),
        }
    }
    Ok(())
}

/// Remove a directory if it exists, tolerating missing paths.
pub fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

/// Recursively copy a directory, preserving file contents.
pub fn copy_dir(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &destination).with_context(|| {
                format!(
                    "copying {} to {}",
                    entry.path().display(),
                    destination.display()
                )
            })?;
        }
        // Symlinks are intentionally not followed when copying.
    }
    Ok(())
}

/// Mark a file executable on Unix; a no-op elsewhere.
pub fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("reading mode of {}", path.display()))?
            .permissions();
        let mode = permissions.mode();
        permissions.set_mode(mode | 0o755);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("setting mode of {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Render a byte count for humans.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render a duration in milliseconds compactly.
pub fn human_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) as f64 / 1000.0;
        format!("{minutes}m {seconds:.1}s")
    }
}

/// Total size of the files under a directory, ignoring unreadable entries.
pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total += dir_size(&entry.path());
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_relative_paths() {
        let base = Path::new("/tmp/base");
        assert!(safe_join(base, "a/b.txt").is_ok());
        assert!(safe_join(base, "../escape").is_err());
        assert!(safe_join(base, "/etc/passwd").is_err());
        assert!(safe_join(base, "a/../../b").is_err());
        #[cfg(windows)]
        assert!(safe_join(base, "C:/windows").is_err());
    }

    #[test]
    fn atomic_write_replaces_contents() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("nested/file.txt");
        atomic_write(&file, b"first").unwrap();
        atomic_write(&file, b"second").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "second");
    }

    #[test]
    fn hashing_matches_known_digest() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("data.txt");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha1_file(&file).unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_bytes(b"abc"), sha256_file(&file).unwrap());
    }

    #[test]
    fn human_readable_helpers() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_duration(500), "500 ms");
        assert_eq!(human_duration(1500), "1.50 s");
    }

    #[test]
    fn copies_directory_trees() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub/file.txt"), b"hi").unwrap();
        let target = temp.path().join("target");
        copy_dir(&source, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("sub/file.txt")).unwrap(),
            "hi"
        );
    }
}
