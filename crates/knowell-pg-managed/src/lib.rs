//! Managed PostgreSQL: the zero-setup database mode where `know` installs
//! and runs its own loopback-only PostgreSQL instance.
//!
//! [`ManagedPostgres`] downloads and verifies PostgreSQL binaries, initialises
//! a cluster under `<knowell_home>/pg/<major>/data`, starts it on
//! `127.0.0.1` at a persisted free port, protects its random superuser
//! password, installs a CI-built pgvector bundle, and offers backup, restore
//! and `pg_upgrade`-based major upgrades. See the crate README for the file
//! layout, security notes and the pgvector bundle layout.
//!
//! ```no_run
//! # async fn demo() -> Result<(), knowell_pg_managed::Error> {
//! use knowell_pg_managed::{ManagedConfig, ManagedPostgres};
//! use secrecy::ExposeSecret;
//!
//! let pg = ManagedPostgres::new(ManagedConfig::new("/home/me/.knowell"))?;
//! pg.install().await?;
//! pg.init_data_dir().await?;
//! pg.start().await?;
//! pg.ensure_database("knowell").await?;
//! let url = pg.connection_url("knowell")?; // secret: never log it
//! # let _ = url.expose_secret();
//! pg.stop().await?;
//! # Ok(()) }
//! ```

mod error;
mod extension;
mod install;
mod layout;
mod manager;
mod ops;
mod password;
mod process;
mod state;

pub use error::{Error, Result};
pub use install::{DEFAULT_MAJOR, MIN_MAJOR};
pub use layout::Layout;
pub use manager::{LISTEN_ADDRESS, ManagedConfig, ManagedPostgres, Status};
pub use password::{
    FilePasswordStore, PASSWORD_LEN, PasswordStore, generate_password, load_or_create,
};
pub use postgresql_archive::Version;
pub use state::{State, pick_free_port, port_is_free};
