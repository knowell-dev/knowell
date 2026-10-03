//! Installing a prebuilt pgvector bundle into the managed distribution.
//!
//! # Bundle layout
//!
//! Knowell's release CI builds pgvector once per platform and PostgreSQL major
//! version against the headers of the distribution that [`install`] downloads,
//! and publishes this directory layout:
//!
//! ```text
//! <bundle>/
//!   share/extension/vector.control     required; `default_version = 'X.Y.Z'`
//!   share/extension/vector--X.Y.Z.sql  required; the script for default_version
//!   share/extension/vector--A--B.sql   optional upgrade scripts (any number)
//!   lib/vector.so | vector.dylib | vector.dll
//!                             required; the file for the target platform
//!                             (Linux .so, macOS .dylib or .so, Windows .dll)
//! ```
//!
//! Other files (licence, build manifest, `bitcode/`) are ignored and not
//! copied. The legacy flat layout is also accepted. Mixed layouts and links in
//! selected files or their bundle directories are rejected before reading them.
//!
//! [`install`]: crate::ManagedPostgres::install

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Upper bound for control and script files; real ones are a few hundred KiB.
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;

/// Library file names accepted on this platform, most preferred first.
fn library_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["vector.dll"]
    } else if cfg!(target_os = "macos") {
        &["vector.dylib", "vector.so"]
    } else {
        &["vector.so"]
    }
}

/// A validated bundle, ready to be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bundle {
    /// `default_version` from the control file.
    pub(crate) version: String,
    /// The shared library.
    pub(crate) library: PathBuf,
    /// `vector.control`.
    pub(crate) control: PathBuf,
    /// All `vector--*.sql` scripts, sorted by file name.
    pub(crate) scripts: Vec<PathBuf>,
}

/// Extract `default_version` from the text of a control file.
///
/// Accepts `default_version = '0.8.0'` with optional spaces; comments (`#`)
/// are ignored. The value must be a plain version string.
pub(crate) fn parse_default_version(control: &str) -> Option<String> {
    for line in control.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "default_version" {
            continue;
        }
        let value = value.trim().trim_matches('\'').trim_matches('"');
        if is_safe_version(value) {
            return Some(value.to_string());
        }
        return None;
    }
    None
}

/// A version usable inside a file name: non-empty, short, `[0-9A-Za-z._]`.
fn is_safe_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// Whether `name` is a script file name this crate will copy: `vector--…--….sql`
/// made of safe characters only.
fn is_script_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("vector--")
        .and_then(|rest| rest.strip_suffix(".sql"))
    else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 80
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn regular_file_size(path: &Path) -> Result<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !is_link(&meta) => Ok(Some(meta.len())),
        Ok(_) => Err(Error::Bundle(format!(
            "{} must be a regular file (not a link or directory)",
            path.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::io(format!("inspecting {}", path.display()), err)),
    }
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject junctions and other reparse points as well as symbolic links.
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn require_directory(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|err| Error::io(format!("inspecting {}", path.display()), err))?;
    if !meta.is_dir() || is_link(&meta) {
        return Err(Error::Bundle(format!(
            "{} must be a directory (not a link or file)",
            path.display()
        )));
    }
    Ok(())
}

fn member_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::io(format!("inspecting {}", path.display()), err)),
    }
}

/// Select one complete layout; never combine files from different layouts.
fn bundle_directories(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    require_directory(dir)?;
    let lib = dir.join("lib");
    let share = dir.join("share");
    if !member_exists(&lib)? && !member_exists(&share)? {
        return Ok((dir.to_path_buf(), dir.to_path_buf()));
    }
    if member_exists(&dir.join("vector.control"))? {
        return Err(Error::Bundle(
            "mixed flat and release bundle layouts; provide exactly one layout".to_string(),
        ));
    }
    require_directory(&lib)?;
    require_directory(&share)?;
    let ext = share.join("extension");
    require_directory(&ext)?;
    Ok((lib, ext))
}

/// Check that `dir` holds a complete bundle for this platform.
///
/// # Errors
/// [`Error::Bundle`] describing the first problem found.
pub(crate) fn validate_bundle(dir: &Path) -> Result<Bundle> {
    let (lib, ext) = bundle_directories(dir)?;
    let control = ext.join("vector.control");
    match regular_file_size(&control)? {
        None => return Err(Error::Bundle("vector.control is missing".to_string())),
        Some(size) if size > MAX_TEXT_BYTES => {
            return Err(Error::Bundle(
                "vector.control is implausibly large".to_string(),
            ));
        }
        Some(_) => {}
    }
    let control_text = std::fs::read_to_string(&control)
        .map_err(|err| Error::io(format!("reading {}", control.display()), err))?;
    let version = parse_default_version(&control_text)
        .ok_or_else(|| Error::Bundle("vector.control has no usable default_version".to_string()))?;

    let mut library = None;
    for name in library_names() {
        let candidate = lib.join(name);
        if regular_file_size(&candidate)?.is_some() {
            library = Some(candidate);
            break;
        }
    }
    let library = library.ok_or_else(|| {
        Error::Bundle(format!(
            "no shared library found; expected {}",
            library_names().join(" or ")
        ))
    })?;

    let mut scripts = Vec::new();
    let entries = std::fs::read_dir(&ext)
        .map_err(|err| Error::io(format!("listing {}", ext.display()), err))?;
    for entry in entries {
        let entry = entry.map_err(|err| Error::io(format!("listing {}", ext.display()), err))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_script_name(name) {
            continue;
        }
        let path = entry.path();
        match regular_file_size(&path)? {
            Some(size) if size > MAX_TEXT_BYTES => {
                return Err(Error::Bundle(format!("{name} is implausibly large")));
            }
            Some(_) => scripts.push(path),
            None => {}
        }
    }
    scripts.sort();
    let default_script = format!("vector--{version}.sql");
    let has_default = scripts
        .iter()
        .any(|p| p.file_name().is_some_and(|n| n == default_script.as_str()));
    if !has_default {
        return Err(Error::Bundle(format!(
            "{default_script} is missing (control file says default_version = {version})"
        )));
    }

    Ok(Bundle {
        version,
        library,
        control,
        scripts,
    })
}

/// Copy `source` to `dest_dir/name` via a temporary name, so a reader never
/// sees a partial file.
fn copy_atomic(source: &Path, dest_dir: &Path, name: &std::ffi::OsStr) -> Result<()> {
    let final_path = dest_dir.join(name);
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".knowell-tmp");
    let tmp_path = dest_dir.join(tmp_name);
    std::fs::copy(source, &tmp_path).map_err(|err| {
        Error::io(
            format!("copying {} to {}", source.display(), tmp_path.display()),
            err,
        )
    })?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp_path);
        Error::io(format!("installing {}", final_path.display()), err)
    })
}

/// Copy a validated bundle into `pkglibdir` and `<sharedir>/extension`.
///
/// The library goes first so the control file never advertises an extension
/// whose library is not yet in place.
///
/// # Errors
/// [`Error::Io`] on copy failure.
pub(crate) fn install_bundle(
    bundle: &Bundle,
    pkglibdir: &Path,
    extension_dir: &Path,
) -> Result<()> {
    std::fs::create_dir_all(extension_dir)
        .map_err(|err| Error::io(format!("creating {}", extension_dir.display()), err))?;
    std::fs::create_dir_all(pkglibdir)
        .map_err(|err| Error::io(format!("creating {}", pkglibdir.display()), err))?;

    // The server loads `$libdir/vector` and appends the platform suffix itself
    // (`.so` on macOS too for PG <16 builds, `.dylib` otherwise), so keep the
    // bundle's own file name.
    if let Some(name) = bundle.library.file_name() {
        copy_atomic(&bundle.library, pkglibdir, name)?;
    }
    for script in &bundle.scripts {
        if let Some(name) = script.file_name() {
            copy_atomic(script, extension_dir, name)?;
        }
    }
    if let Some(name) = bundle.control.file_name() {
        copy_atomic(&bundle.control, extension_dir, name)?;
    }
    Ok(())
}

/// Validate an extension identifier (`pg_available_extensions.name`) before it
/// is embedded in SQL: 1-63 characters of `[a-z0-9_]`, not starting with a digit.
pub(crate) fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 63
        && (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib_name() -> &'static str {
        library_names().first().copied().unwrap()
    }

    fn make_bundle(dir: &Path, version: &str) {
        std::fs::write(
            dir.join("vector.control"),
            format!("comment = 'vector data type'\ndefault_version = '{version}'\n"),
        )
        .unwrap();
        std::fs::write(dir.join(lib_name()), b"\x7fELF-fake").unwrap();
        std::fs::write(dir.join(format!("vector--{version}.sql")), "-- sql").unwrap();
        std::fs::write(dir.join("vector--0.7.0--0.8.0.sql"), "-- upgrade").unwrap();
        std::fs::write(dir.join("LICENSE"), "ignored").unwrap();
    }

    fn make_release_bundle(dir: &Path) {
        let ext = dir.join("share").join("extension");
        let lib = dir.join("lib");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(ext.join("vector.control"), "default_version = '0.8.7'").unwrap();
        std::fs::write(ext.join("vector--0.8.7.sql"), "-- sql").unwrap();
        std::fs::write(ext.join("other.control"), "ignored").unwrap();
        std::fs::write(lib.join(lib_name()), "fake library").unwrap();
        std::fs::write(dir.join("manifest.json"), "{}").unwrap();
    }

    // These three cases previously exercised the CLI's flattening adapter.
    // Keeping them here verifies the shared installer used by init and upgrade.
    #[test]
    fn flat_bundles_are_used_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        make_bundle(dir.path(), "0.8.7");
        let bundle = validate_bundle(dir.path()).unwrap();
        assert_eq!(bundle.control, dir.path().join("vector.control"));
        assert_eq!(bundle.library, dir.path().join(lib_name()));
    }

    #[test]
    fn release_bundles_are_installed_directly() {
        let dir = tempfile::tempdir().unwrap();
        make_release_bundle(dir.path());
        let bundle = validate_bundle(dir.path()).unwrap();
        assert_eq!(bundle.version, "0.8.7");
        assert_eq!(
            bundle.control,
            dir.path().join("share/extension/vector.control")
        );
        assert_eq!(bundle.library, dir.path().join("lib").join(lib_name()));
        let dest = tempfile::tempdir().unwrap();
        let lib = dest.path().join("lib");
        let ext = dest.path().join("share/extension");
        for _ in 0..2 {
            install_bundle(&bundle, &lib, &ext).unwrap();
        }
        let mut names: Vec<_> = [&lib, &ext]
            .into_iter()
            .flat_map(|path| std::fs::read_dir(path).unwrap())
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let mut expected = vec!["vector--0.8.7.sql", "vector.control", lib_name()];
        expected.sort();
        assert_eq!(names, expected);
        assert_eq!(
            std::fs::read(ext.join("vector--0.8.7.sql")).unwrap(),
            b"-- sql"
        );
        assert_eq!(
            std::fs::read(lib.join(lib_name())).unwrap(),
            b"fake library"
        );
    }

    #[test]
    fn non_bundles_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_bundle(dir.path()).is_err());
    }

    #[test]
    fn release_layout_must_be_complete_and_unambiguous() {
        for missing in [
            PathBuf::from("lib").join(lib_name()),
            PathBuf::from("share/extension/vector.control"),
            PathBuf::from("share/extension/vector--0.8.7.sql"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            make_release_bundle(dir.path());
            std::fs::remove_file(dir.path().join(missing)).unwrap();
            assert!(validate_bundle(dir.path()).is_err());
        }
        let mixed = tempfile::tempdir().unwrap();
        make_release_bundle(mixed.path());
        make_bundle(mixed.path(), "0.8.0");
        let error = validate_bundle(mixed.path()).unwrap_err();
        assert!(error.to_string().contains("mixed flat and release"));
    }

    #[test]
    fn release_directories_cannot_be_files() {
        for member in ["lib", "share", "share/extension"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(member);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "not a directory").unwrap();
            assert!(validate_bundle(dir.path()).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_release_directories_and_members_are_rejected() {
        for member in ["lib", "share", "share/extension"] {
            let dir = tempfile::tempdir().unwrap();
            make_release_bundle(dir.path());
            let source = dir.path().join(member);
            let moved = dir.path().join("moved");
            std::fs::rename(&source, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &source).unwrap();
            assert!(matches!(validate_bundle(dir.path()), Err(Error::Bundle(_))));
        }
        for member in [
            PathBuf::from("lib").join(lib_name()),
            PathBuf::from("share/extension/vector.control"),
            PathBuf::from("share/extension/vector--0.8.7.sql"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            make_release_bundle(dir.path());
            let source = dir.path().join(member);
            let moved = dir.path().join("moved");
            std::fs::rename(&source, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &source).unwrap();
            assert!(matches!(validate_bundle(dir.path()), Err(Error::Bundle(_))));
        }
    }

    #[test]
    fn parses_default_version_variants() {
        assert_eq!(
            parse_default_version("default_version = '0.8.0'\n"),
            Some("0.8.0".to_string())
        );
        assert_eq!(
            parse_default_version("# c\n  default_version='1.2.3'  # trailing\n"),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            parse_default_version("default_version = \"0.8.1\""),
            Some("0.8.1".to_string())
        );
    }

    #[test]
    fn rejects_bad_default_versions() {
        for bad in [
            "",
            "comment = 'x'",
            "default_version = ''",
            "default_version = '../../etc'",
            "default_version = '1.0/evil'",
            "default_version = 'a b'",
            "default_version",
            &format!("default_version = '{}'", "9".repeat(64)),
        ] {
            assert_eq!(parse_default_version(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn accepts_complete_bundle() {
        let dir = tempfile::tempdir().unwrap();
        make_bundle(dir.path(), "0.8.0");
        let bundle = validate_bundle(dir.path()).unwrap();
        assert_eq!(bundle.version, "0.8.0");
        assert_eq!(bundle.scripts.len(), 2);
        assert!(bundle.library.ends_with(lib_name()));
    }

    #[test]
    fn rejects_incomplete_bundles() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_bundle(&dir.path().join("missing")).is_err());

        // empty dir
        assert!(matches!(validate_bundle(dir.path()), Err(Error::Bundle(_))));

        // missing library
        make_bundle(dir.path(), "0.8.0");
        std::fs::remove_file(dir.path().join(lib_name())).unwrap();
        let err = validate_bundle(dir.path()).unwrap_err();
        assert!(err.to_string().contains("shared library"));

        // missing default script
        make_bundle(dir.path(), "0.8.0");
        std::fs::remove_file(dir.path().join("vector--0.8.0.sql")).unwrap();
        let err = validate_bundle(dir.path()).unwrap_err();
        assert!(err.to_string().contains("vector--0.8.0.sql"));

        // missing control
        make_bundle(dir.path(), "0.8.0");
        std::fs::remove_file(dir.path().join("vector.control")).unwrap();
        assert!(validate_bundle(dir.path()).is_err());
    }

    #[test]
    fn control_without_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        make_bundle(dir.path(), "0.8.0");
        std::fs::write(dir.path().join("vector.control"), "comment = 'x'\n").unwrap();
        let err = validate_bundle(dir.path()).unwrap_err();
        assert!(err.to_string().contains("default_version"));
    }

    #[test]
    fn script_names_are_filtered() {
        assert!(is_script_name("vector--0.8.0.sql"));
        assert!(is_script_name("vector--0.7.0--0.8.0.sql"));
        assert!(!is_script_name("vector--.sql"));
        assert!(!is_script_name("vector--a b.sql"));
        assert!(!is_script_name("vector--..%2f.sql"));
        assert!(!is_script_name("other--1.0.sql"));
        assert!(!is_script_name("vector--1.0.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_members_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        make_bundle(dir.path(), "0.8.0");
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "x").unwrap();
        std::fs::remove_file(dir.path().join("vector--0.8.0.sql")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("vector--0.8.0.sql")).unwrap();
        assert!(matches!(validate_bundle(dir.path()), Err(Error::Bundle(_))));
    }

    #[test]
    fn installs_files_into_target_dirs() {
        let src = tempfile::tempdir().unwrap();
        make_bundle(src.path(), "0.8.0");
        let bundle = validate_bundle(src.path()).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let lib = dst.path().join("lib");
        let ext = dst.path().join("share").join("extension");
        install_bundle(&bundle, &lib, &ext).unwrap();
        assert!(lib.join(lib_name()).is_file());
        assert!(ext.join("vector.control").is_file());
        assert!(ext.join("vector--0.8.0.sql").is_file());
        assert!(ext.join("vector--0.7.0--0.8.0.sql").is_file());
        assert!(!ext.join("LICENSE").exists());
        // idempotent
        install_bundle(&bundle, &lib, &ext).unwrap();
    }

    #[test]
    fn identifier_validation() {
        for ok in ["vector", "my_db", "_x", "a1"] {
            assert!(is_valid_identifier(ok), "{ok}");
        }
        for bad in ["", "1a", "A", "a-b", "a b", "a\"b", "a;b", &"a".repeat(64)] {
            assert!(!is_valid_identifier(bad), "{bad}");
        }
    }
}
