//! Downloading and caching PostgreSQL distributions.
//!
//! Binaries come from the `theseus-rs/postgresql-binaries` GitHub releases via
//! `postgresql_archive`, which verifies the published SHA-256 of every archive
//! before it is unpacked.

use crate::error::{Error, Result};
use crate::layout::Layout;
use postgresql_archive::configuration::theseus;
use postgresql_archive::{Version, VersionReq};
use std::path::{Path, PathBuf};

/// Lowest PostgreSQL major version this crate manages.
pub const MIN_MAJOR: u32 = 15;

/// Default PostgreSQL major version.
pub const DEFAULT_MAJOR: u32 = 17;

/// Installed distributions for `major` below `dist_root`, ascending by version.
///
/// Directory names that are not valid semantic versions are ignored.
pub(crate) fn installed_versions(dist_root: &Path, major: u32) -> Result<Vec<(Version, PathBuf)>> {
    let entries = match std::fs::read_dir(dist_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(Error::io(format!("listing {}", dist_root.display()), err));
        }
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|err| Error::io(format!("listing {}", dist_root.display()), err))?;
        let name = entry.file_name();
        let Ok(version) = Version::parse(&name.to_string_lossy()) else {
            continue;
        };
        if version.major == u64::from(major) && entry.path().join("bin").is_dir() {
            found.push((version, entry.path()));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// The newest installed distribution of `major`, if any.
pub(crate) fn newest_installed(layout: &Layout) -> Result<Option<(Version, PathBuf)>> {
    Ok(installed_versions(&layout.dist_root(), layout.major())?.pop())
}

/// Ensure a distribution for `layout.major()` is cached; download one if not.
///
/// Returns the version and its directory. An existing cached minor version is
/// reused without contacting the network.
///
/// # Errors
/// [`Error::Download`] if resolving, downloading, verifying or unpacking fails;
/// [`Error::Io`] for local filesystem problems.
pub(crate) async fn ensure_installed(layout: &Layout) -> Result<(Version, PathBuf)> {
    if let Some(found) = newest_installed(layout)? {
        return Ok(found);
    }
    let major = layout.major();
    let fail = |message: String| Error::Download {
        major,
        message: if message.contains("403") || message.contains("429") {
            format!(
                "{message}; the unauthenticated GitHub API limit may be exhausted, set GITHUB_TOKEN to raise it"
            )
        } else {
            message
        },
    };
    let wanted = VersionReq::parse(&format!("={major}")).map_err(|e| fail(e.to_string()))?;

    // One call resolves the newest release of this major and downloads it; the
    // archive's SHA-256 is verified inside `get_archive`. Each call pages
    // through the GitHub releases API, which is why the cache is checked first.
    tracing::info!(major, "downloading postgresql");
    let (version, bytes) = postgresql_archive::get_archive(theseus::URL, &wanted)
        .await
        .map_err(|e| fail(e.to_string()))?;
    if version.major != u64::from(major) {
        return Err(fail(format!(
            "release {version} does not match the requested major"
        )));
    }

    let dist_root = layout.dist_root();
    std::fs::create_dir_all(&dist_root)
        .map_err(|e| Error::io(format!("creating {}", dist_root.display()), e))?;
    let target = dist_root.join(version.to_string());
    if target.join("bin").is_dir() {
        return Ok((version, target));
    }

    // Unpack into a private staging directory, then rename: a crash never
    // leaves a half-extracted tree that looks installed.
    let staging = dist_root.join(format!(".staging-{version}-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|e| Error::io(format!("clearing {}", staging.display()), e))?;
    }
    let staging_for_task = staging.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        // `extract` is async in signature but performs only blocking file work.
        tokio::runtime::Handle::current().block_on(postgresql_archive::extract(
            theseus::URL,
            &bytes,
            &staging_for_task,
        ))
    })
    .await
    .map_err(|e| fail(format!("extraction task failed: {e}")))?;
    if let Err(err) = extracted {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(fail(err.to_string()));
    }
    if !staging.join("bin").is_dir() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(fail(
            "archive has an unexpected layout (no bin directory)".to_string(),
        ));
    }
    if let Err(err) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_dir_all(&staging);
        // A concurrent installer may have won the race; that is fine.
        if target.join("bin").is_dir() {
            return Ok((version, target));
        }
        return Err(Error::io(format!("installing {}", target.display()), err));
    }
    tracing::info!(%version, "postgresql installed");
    Ok((version, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_dist(root: &Path, name: &str) {
        std::fs::create_dir_all(root.join(name).join("bin")).unwrap();
    }

    #[test]
    fn lists_only_matching_major_in_ascending_order() {
        let dir = tempfile::tempdir().unwrap();
        fake_dist(dir.path(), "17.2.0");
        fake_dist(dir.path(), "17.10.1");
        fake_dist(dir.path(), "18.0.0");
        fake_dist(dir.path(), "not-a-version");
        std::fs::create_dir_all(dir.path().join("17.9.9")).unwrap(); // no bin: incomplete
        let found = installed_versions(dir.path(), 17).unwrap();
        let versions: Vec<String> = found.iter().map(|(v, _)| v.to_string()).collect();
        assert_eq!(versions, ["17.2.0", "17.10.1"]);
    }

    #[test]
    fn missing_dist_root_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            installed_versions(&dir.path().join("none"), 17)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn newest_installed_picks_highest() {
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::new(home.path(), 17);
        fake_dist(&layout.dist_root(), "17.1.0");
        fake_dist(&layout.dist_root(), "17.5.0");
        let (version, _) = newest_installed(&layout).unwrap().unwrap();
        assert_eq!(version.to_string(), "17.5.0");
        assert!(
            newest_installed(&Layout::new(home.path(), 16))
                .unwrap()
                .is_none()
        );
    }
}
