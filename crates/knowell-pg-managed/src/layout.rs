//! On-disk layout of the managed installation.
//!
//! ```text
//! <knowell_home>/pg/
//!   dist/<x.y.z>/        PostgreSQL binaries (shared by all data dirs of that version)
//!   password             superuser password, owner-only (file PasswordStore)
//!   tmp/                 short-lived credential files for child processes
//!   <major>/
//!     data/              the cluster (PGDATA)
//!     state.json         chosen loopback port
//!     postgres.log       server log
//! ```

use std::path::{Path, PathBuf};

/// Resolved paths for one major version under one Knowell home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pg_root: PathBuf,
    major: u32,
}

impl Layout {
    /// Build the layout for `major` below `knowell_home` (the directory
    /// that contains `pg/`). No filesystem access happens here.
    #[must_use]
    pub fn new(knowell_home: &Path, major: u32) -> Self {
        Self {
            pg_root: knowell_home.join("pg"),
            major,
        }
    }

    /// The PostgreSQL major version this layout belongs to.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.major
    }

    /// `<knowell_home>/pg`.
    #[must_use]
    pub fn pg_root(&self) -> &Path {
        &self.pg_root
    }

    /// Cache of downloaded distributions: `<knowell_home>/pg/dist`.
    #[must_use]
    pub fn dist_root(&self) -> PathBuf {
        self.pg_root.join("dist")
    }

    /// Superuser password file: `<knowell_home>/pg/password`. Shared by all
    /// major versions so an upgraded cluster keeps the same credentials.
    #[must_use]
    pub fn password_file(&self) -> PathBuf {
        self.pg_root.join("password")
    }

    /// Scratch directory for credential files handed to child processes.
    #[must_use]
    pub fn tmp_dir(&self) -> PathBuf {
        self.pg_root.join("tmp")
    }

    /// `<knowell_home>/pg/<major>`.
    #[must_use]
    pub fn major_dir(&self) -> PathBuf {
        self.pg_root.join(self.major.to_string())
    }

    /// Cluster directory: `<knowell_home>/pg/<major>/data`.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.major_dir().join("data")
    }

    /// Small JSON state file with the chosen port.
    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.major_dir().join("state.json")
    }

    /// Server log (stdout/stderr of the postmaster).
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.major_dir().join("postgres.log")
    }

    /// Server pid file inside the data directory.
    #[must_use]
    pub fn postmaster_pid(&self) -> PathBuf {
        self.data_dir().join("postmaster.pid")
    }

    /// Marker written by `initdb`; present once the cluster is initialised.
    #[must_use]
    pub fn pg_version_file(&self) -> PathBuf {
        self.data_dir().join("PG_VERSION")
    }

    /// Server settings managed by Knowell, included from `postgresql.conf`.
    #[must_use]
    pub fn managed_conf(&self) -> PathBuf {
        self.data_dir().join("knowell.conf")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn paths_follow_documented_layout() {
        let layout = Layout::new(Path::new("home"), 17);
        assert_eq!(layout.pg_root(), Path::new("home").join("pg"));
        assert_eq!(
            layout.dist_root(),
            Path::new("home").join("pg").join("dist")
        );
        assert_eq!(
            layout.data_dir(),
            Path::new("home").join("pg").join("17").join("data")
        );
        assert_eq!(
            layout.state_file(),
            Path::new("home").join("pg").join("17").join("state.json")
        );
        assert_eq!(
            layout.password_file(),
            Path::new("home").join("pg").join("password")
        );
        assert_eq!(
            layout.postmaster_pid(),
            Path::new("home")
                .join("pg")
                .join("17")
                .join("data")
                .join("postmaster.pid")
        );
        assert_eq!(layout.major(), 17);
    }

    #[test]
    fn majors_do_not_share_data_dirs() {
        let a = Layout::new(Path::new("h"), 17);
        let b = Layout::new(Path::new("h"), 18);
        assert_ne!(a.data_dir(), b.data_dir());
        assert_eq!(a.password_file(), b.password_file());
        assert_eq!(a.dist_root(), b.dist_root());
    }
}
