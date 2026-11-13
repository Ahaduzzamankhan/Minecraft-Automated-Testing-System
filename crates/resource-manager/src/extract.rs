//! Safe archive extraction for natives and Java runtimes.
//!
//! Every entry is checked before it touches the filesystem: absolute paths,
//! parent traversal, drive prefixes and links are rejected. Nothing is ever
//! extracted outside the requested destination directory.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mats_core::fsutil;
use zip::ZipArchive;

/// Extract a ZIP archive into `destination`, skipping symlinks and rejecting
/// unsafe paths.
pub fn extract_zip_safely(archive_path: &Path, destination: &Path) -> Result<()> {
    let file =
        File::open(archive_path).with_context(|| format!("opening {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("reading {} as a ZIP archive", archive_path.display()))?;
    fs::create_dir_all(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading entry #{index} of {}", archive_path.display()))?;
        let Some(relative) = entry.enclosed_name() else {
            bail!(
                "archive {} contains an unsafe path `{}`",
                archive_path.display(),
                entry.name()
            );
        };
        fsutil::validate_relative(&relative)?;
        let target = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
            continue;
        }
        #[cfg(unix)]
        {
            // Reject symbolic links encoded through the Unix mode bits.
            if entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                bail!(
                    "archive {} contains a symbolic link `{}`",
                    archive_path.display(),
                    entry.name()
                );
            }
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut output =
            File::create(&target).with_context(|| format!("creating {}", target.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("extracting {}", target.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                // Preserve the executable bit only; never preserve setuid bits.
                if mode & 0o111 != 0 {
                    let permissions = fs::Permissions::from_mode(0o755);
                    let _ = fs::set_permissions(&target, permissions);
                }
            }
        }
    }
    Ok(())
}

/// Create a symlink from a downloaded archive, refusing targets that escape
/// the destination directory. Official JDK tarballs legitimately contain
/// relative symlinks, which is why they cannot simply be rejected.
fn link_inside(destination: &Path, target: &Path, link: &Path) -> Result<()> {
    use std::path::Component;
    if link.is_absolute() {
        bail!(
            "archive symlink `{}` is absolute and cannot be extracted safely",
            link.display()
        );
    }
    let base = target.parent().unwrap_or(destination);
    // Resolve `..` lexically. Adoptium's JDK tarballs contain links such as
    // `legal/jdk.nio.mapmode/ASSEMBLY_EXCEPTION -> ../java.base/ASSEMBLY_EXCEPTION`,
    // which are legitimate as long as they stay inside the destination.
    let mut resolved = base.to_path_buf();
    for component in link.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    bail!(
                        "archive symlink `{}` resolves outside the destination",
                        link.display()
                    );
                }
            }
            Component::Normal(part) => resolved.push(part),
            _ => bail!(
                "archive symlink `{}` resolves outside the destination",
                link.display()
            ),
        }
    }
    if !resolved.starts_with(destination) {
        bail!(
            "archive symlink `{}` resolves outside the destination",
            link.display()
        );
    }
    let source = resolved;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(link, target)
            .with_context(|| format!("creating symlink {}", target.display()))?;
    }
    #[cfg(not(unix))]
    {
        // Windows cannot create every Unix symlink without extra privileges;
        // copy the referenced content instead.
        if source.is_file() {
            fs::copy(&source, target)
                .with_context(|| format!("copying {} to {}", source.display(), target.display()))?;
        }
    }
    Ok(())
}

/// Extract a `.tar.gz` archive, optionally stripping leading path components.
///
/// Regular files, directories and links are extracted; links must resolve
/// inside the destination. Device nodes and FIFOs are refused because a
/// downloaded archive has no legitimate reason to contain them.
pub fn extract_tar_gz_safely(
    archive_path: &Path,
    destination: &Path,
    strip_components: usize,
) -> Result<()> {
    let file =
        File::open(archive_path).with_context(|| format!("opening {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    fs::create_dir_all(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    for entry in archive
        .entries()
        .with_context(|| format!("reading tar entries from {}", archive_path.display()))?
    {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        fsutil::validate_relative(&path)?;
        let kind = entry.header().entry_type();
        #[cfg(unix)]
        let mode = entry.header().mode().unwrap_or(0o644);
        let stripped: PathBuf = path
            .components()
            .skip(strip_components)
            .collect::<PathBuf>();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        fsutil::validate_relative(&stripped)?;
        let target = destination.join(&stripped);
        if kind.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
            continue;
        }
        if !(kind.is_file() || kind.is_symlink() || kind.is_hard_link()) {
            bail!(
                "archive {} contains an unsupported entry type for `{}`",
                archive_path.display(),
                path.display()
            );
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if kind.is_symlink() {
            let link = entry.link_name()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "archive {} contains a symlink without a target for `{}`",
                    archive_path.display(),
                    path.display()
                )
            })?;
            link_inside(destination, &target, &link)?;
            continue;
        }
        if kind.is_hard_link() {
            let link = entry.link_name()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "archive {} contains a hard link without a target for `{}`",
                    archive_path.display(),
                    path.display()
                )
            })?;
            let source_rel: PathBuf = link.components().skip(strip_components).collect();
            fsutil::validate_relative(&source_rel)?;
            let source = destination.join(&source_rel);
            if fs::hard_link(&source, &target).is_err() {
                fs::copy(&source, &target).with_context(|| {
                    format!("linking {} to {}", source.display(), target.display())
                })?;
            }
            continue;
        }
        let mut output =
            File::create(&target).with_context(|| format!("creating {}", target.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("extracting {}", target.display()))?;
        #[cfg(unix)]
        {
            if mode & 0o111 != 0 {
                let _ = fsutil::set_executable(&target);
            }
        }
    }
    Ok(())
}

/// Extract a ZIP archive while flattening its single top-level directory.
///
/// Used for Java runtime archives, which wrap everything in one directory.
pub fn extract_zip_flattened(archive_path: &Path, destination: &Path) -> Result<()> {
    let staging = destination.with_extension("staging");
    fsutil::remove_dir_if_exists(&staging)?;
    extract_zip_safely(archive_path, &staging)?;
    let entries: Vec<PathBuf> = fs::read_dir(&staging)
        .with_context(|| format!("listing {}", staging.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    let source = match entries.as_slice() {
        [single] if single.is_dir() => single.clone(),
        _ => staging.clone(),
    };
    if source == staging {
        fsutil::remove_dir_if_exists(destination)?;
        fs::rename(&staging, destination).with_context(|| {
            format!("moving {} to {}", staging.display(), destination.display())
        })?;
    } else {
        fsutil::remove_dir_if_exists(destination)?;
        fs::rename(&source, destination)
            .with_context(|| format!("moving {} to {}", source.display(), destination.display()))?;
        fsutil::remove_dir_if_exists(&staging)?;
    }
    Ok(())
}

/// Extract a `.tar.gz` while flattening its single top-level directory.
pub fn extract_tar_gz_flattened(archive_path: &Path, destination: &Path) -> Result<()> {
    let staging = destination.with_extension("staging-tar");
    fsutil::remove_dir_if_exists(&staging)?;
    extract_tar_gz_safely(archive_path, &staging, 0)?;
    let entries: Vec<PathBuf> = fs::read_dir(&staging)
        .with_context(|| format!("listing {}", staging.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    let source = match entries.as_slice() {
        [single] if single.is_dir() => single.clone(),
        _ => staging.clone(),
    };
    fsutil::remove_dir_if_exists(destination)?;
    fs::rename(&source, destination)
        .with_context(|| format!("moving {} to {}", source.display(), destination.display()))?;
    fsutil::remove_dir_if_exists(&staging)?;
    Ok(())
}

/// Read a single entry from a ZIP archive without extracting anything.
pub fn read_zip_entry(archive_path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    match archive.by_name(name) {
        Ok(mut entry) => {
            let mut buffer = Vec::new();
            entry
                .by_ref()
                .take(32 * 1024 * 1024)
                .read_to_end(&mut buffer)?;
            Ok(Some(buffer))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, contents) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn extracts_safe_zip() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("natives.zip");
        write_zip(
            &archive,
            &[("liblwjgl.so", b"binary"), ("nested/readme.txt", b"text")],
        );
        let destination = temp.path().join("out");
        extract_zip_safely(&archive, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("liblwjgl.so")).unwrap(),
            b"binary"
        );
        assert!(destination.join("nested/readme.txt").is_file());
    }

    #[test]
    fn rejects_traversal_entries() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("evil.zip");
        write_zip(&archive, &[("../escape.txt", b"nope")]);
        let error = extract_zip_safely(&archive, &temp.path().join("out")).unwrap_err();
        assert!(!temp.path().join("escape.txt").exists());
        assert!(
            format!("{error}").to_lowercase().contains("unsafe")
                || format!("{error}").to_lowercase().contains("invalid")
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("evil.zip");
        write_zip(&archive, &[("/tmp/absolute.txt", b"nope")]);
        let error = extract_zip_safely(&archive, &temp.path().join("out")).unwrap_err();
        assert!(format!("{error}").to_lowercase().contains("unsafe"));
    }

    #[test]
    fn flattening_removes_the_wrapper_directory() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("jdk.zip");
        write_zip(
            &archive,
            &[
                ("jdk-25/bin/java", b"#!/bin/sh\n"),
                ("jdk-25/release", b"JAVA_VERSION=25"),
            ],
        );
        let destination = temp.path().join("jdk");
        extract_zip_flattened(&archive, &destination).unwrap();
        assert!(destination.join("bin/java").is_file());
        assert!(destination.join("release").is_file());
    }

    #[test]
    fn tar_extraction_strips_components_and_marks_executables() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("jdk.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "jdk-25/bin/java", io::Cursor::new(b"java"))
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let destination = temp.path().join("out");
        extract_tar_gz_flattened(&archive, &destination).unwrap();
        assert!(destination.join("bin/java").is_file());
        #[cfg(unix)]
        {
            assert!(fsutil::is_executable(&destination.join("bin/java")));
        }
    }

    #[test]
    fn reads_single_entry() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("a.zip");
        write_zip(&archive, &[("version.json", br#"{"id":"26.3"}"#)]);
        let entry = read_zip_entry(&archive, "version.json").unwrap().unwrap();
        assert!(String::from_utf8_lossy(&entry).contains("26.3"));
        assert!(read_zip_entry(&archive, "missing.json").unwrap().is_none());
    }
}
