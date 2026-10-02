//! End-to-end tests that download and run a real PostgreSQL.
//!
//! They are `#[ignore]`d (network, tens of MB, tens of seconds); run them
//! nightly or by hand:
//!
//! ```text
//! python scripts/buildlock.py cargo test -p knowell-pg-managed --test e2e -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used)]

use knowell_pg_managed::{Error, ManagedConfig, ManagedPostgres, Status};
use secrecy::ExposeSecret;

fn manager(home: &std::path::Path, major: u32) -> ManagedPostgres {
    ManagedPostgres::new(ManagedConfig::new(home).with_major(major)).unwrap()
}

#[tokio::test]
#[ignore = "downloads and runs a real PostgreSQL"]
async fn lifecycle_backup_restore() {
    let home = tempfile::tempdir().unwrap();
    let pg = manager(home.path(), 17);

    assert_eq!(pg.status().await.unwrap(), Status::NotInstalled);
    let version = pg.install().await.unwrap();
    assert_eq!(version.major, 17);
    assert_eq!(pg.status().await.unwrap(), Status::Installed);

    assert!(pg.init_data_dir().await.unwrap());
    assert!(!pg.init_data_dir().await.unwrap(), "second init is a no-op");
    assert_eq!(pg.status().await.unwrap(), Status::Stopped);

    let port = pg.start().await.unwrap();
    assert!(matches!(
        pg.status().await.unwrap(),
        Status::Running { port: p, .. } if p == port
    ));
    assert!(matches!(
        pg.start().await,
        Err(Error::AlreadyRunning { .. })
    ));
    assert_eq!(pg.port().unwrap(), Some(port));

    // Loopback only.
    assert_eq!(
        pg.run_sql("postgres", "SHOW listen_addresses")
            .await
            .unwrap(),
        "127.0.0.1"
    );
    assert_eq!(pg.run_sql("postgres", "SELECT 1").await.unwrap(), "1");

    assert!(pg.ensure_database("knowell").await.unwrap());
    assert!(!pg.ensure_database("knowell").await.unwrap());
    assert_eq!(pg.run_sql("knowell", "SELECT 1").await.unwrap(), "1");

    // The URL works and is a secret.
    let url = pg.connection_url("knowell").unwrap();
    assert!(url.expose_secret().starts_with("postgresql://postgres:"));
    assert!(!format!("{url:?}").contains(url.expose_secret()));

    // Extension discovery: contrib extensions yes, pgvector not yet.
    assert!(pg.extension_available("plpgsql").await.unwrap().is_some());
    assert_eq!(pg.extension_available("vector").await.unwrap(), None);

    // Install a (fake) pgvector bundle: the control file is enough for the
    // server to list the extension; creating it would need a real library.
    let bundle = home.path().join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(
        bundle.join("vector.control"),
        "comment = 'fake'
default_version = '0.8.0'
",
    )
    .unwrap();
    std::fs::write(bundle.join("vector--0.8.0.sql"), "-- fake").unwrap();
    let library = if cfg!(windows) {
        "vector.dll"
    } else {
        "vector.so"
    };
    std::fs::write(bundle.join(library), b"fake").unwrap();
    assert_eq!(pg.install_extension_bundle(&bundle).await.unwrap(), "0.8.0");
    assert_eq!(
        pg.extension_available("vector").await.unwrap(),
        Some("0.8.0".to_string())
    );

    // Backup and restore into a fresh database.
    pg.run_sql(
        "knowell",
        "CREATE TABLE notes (id int PRIMARY KEY, body text); \
         INSERT INTO notes VALUES (1, 'hello'), (2, U&'w\\00f6rld')",
    )
    .await
    .unwrap();
    let dump = home.path().join("backups").join("knowell.dump");
    pg.backup("knowell", &dump).await.unwrap();
    assert!(dump.is_file());
    assert!(!dump.with_extension("dump.partial").exists());

    pg.restore(&dump, "knowell_restored").await.unwrap();
    assert!(matches!(
        pg.restore(&dump, "knowell_restored").await,
        Err(Error::DatabaseExists(_))
    ));
    assert_eq!(
        pg.run_sql("knowell_restored", "SELECT count(*) FROM notes")
            .await
            .unwrap(),
        "2"
    );
    assert_eq!(
        pg.run_sql("knowell_restored", "SELECT body FROM notes WHERE id = 2")
            .await
            .unwrap(),
        "wörld"
    );

    // Stop, then simulate a crash: a pid file whose process is gone.
    pg.stop().await.unwrap();
    assert_eq!(pg.status().await.unwrap(), Status::Stopped);
    let pid_file = pg.layout().postmaster_pid();
    std::fs::write(
        &pid_file,
        format!(
            "999999\n{}\n0\n{port}\n\n127.0.0.1\n\n",
            pg.layout().data_dir().display()
        ),
    )
    .unwrap();
    assert!(matches!(
        pg.status().await.unwrap(),
        Status::StalePostmasterPid { pid: 999_999 }
    ));
    assert!(matches!(
        pg.start().await,
        Err(Error::StalePostmasterPid { .. })
    ));
    assert!(pg.clear_stale_postmaster_pid().await.unwrap());

    // Data survives a restart, on the same port.
    assert_eq!(pg.start().await.unwrap(), port);
    assert_eq!(
        pg.run_sql("knowell", "SELECT count(*) FROM notes")
            .await
            .unwrap(),
        "2"
    );
    pg.stop().await.unwrap();
}

#[tokio::test]
#[ignore = "downloads two PostgreSQL majors and runs pg_upgrade"]
async fn upgrade_17_to_18() {
    let home = tempfile::tempdir().unwrap();
    let old = manager(home.path(), 17);
    old.install().await.unwrap();
    old.init_data_dir().await.unwrap();
    old.start().await.unwrap();
    old.ensure_database("knowell").await.unwrap();
    old.run_sql(
        "knowell",
        "CREATE TABLE t (id int); INSERT INTO t VALUES (7)",
    )
    .await
    .unwrap();

    // A running cluster cannot be upgraded.
    assert!(matches!(
        old.upgrade(18, None).await,
        Err(Error::NotStopped)
    ));
    old.stop().await.unwrap();

    let new = old.upgrade(18, None).await.unwrap();
    assert_eq!(new.major(), 18);
    // The old data directory is untouched.
    assert!(old.layout().data_dir().join("PG_VERSION").is_file());

    new.start().await.unwrap();
    assert_eq!(
        new.run_sql("knowell", "SELECT id FROM t").await.unwrap(),
        "7"
    );
    assert!(
        new.run_sql("postgres", "SHOW server_version")
            .await
            .unwrap()
            .starts_with("18")
    );
    new.stop().await.unwrap();

    old.remove_data_dir().await.unwrap();
    assert!(!old.layout().data_dir().exists());
}
