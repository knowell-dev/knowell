use knowell_core::TrackTarget;
use knowell_store::hierarchy::*;
use knowell_store::views;
use knowell_store::{SourceKind, StoreError};

use crate::common::{name, path, require_db};

#[tokio::test]
async fn names_are_unique_per_parent() {
    let db = require_db!();
    let mut c = db.conn().await;

    let acme = create_organization(&mut c, &name("acme")).await.unwrap();
    let globex = create_organization(&mut c, &name("globex")).await.unwrap();
    assert!(matches!(
        create_organization(&mut c, &name("acme")).await,
        Err(StoreError::AlreadyExists {
            entity: "organization",
            ..
        })
    ));

    let ws = create_workspace(&mut c, acme.id, &name("shop"))
        .await
        .unwrap();
    assert!(matches!(
        create_workspace(&mut c, acme.id, &name("shop")).await,
        Err(StoreError::AlreadyExists {
            entity: "workspace",
            ..
        })
    ));
    // Same name under another parent is fine.
    let other_ws = create_workspace(&mut c, globex.id, &name("shop"))
        .await
        .unwrap();
    assert_ne!(ws.id, other_ws.id);

    let src = create_source(&mut c, acme.id, SourceKind::Git, "/repos/shop")
        .await
        .unwrap();
    assert!(matches!(
        create_source(&mut c, acme.id, SourceKind::Directory, "/repos/shop").await,
        Err(StoreError::AlreadyExists {
            entity: "source",
            ..
        })
    ));
    let globex_src = create_source(&mut c, globex.id, SourceKind::Git, "/repos/shop")
        .await
        .unwrap();

    let api = create_project(
        &mut c,
        ws.id,
        src.id,
        &name("api"),
        Some(&path("services/api")),
    )
    .await
    .unwrap();
    assert_eq!(api.root, Some(path("services/api")));
    assert_eq!(api.organization, acme.id);
    assert!(matches!(
        create_project(&mut c, ws.id, src.id, &name("api"), None).await,
        Err(StoreError::AlreadyExists {
            entity: "project",
            ..
        })
    ));
    let web = create_project(&mut c, ws.id, src.id, &name("web"), None)
        .await
        .unwrap();
    assert_eq!(web.root, None);

    // A project cannot use a source of another organization.
    let err = create_project(&mut c, ws.id, globex_src.id, &name("leak"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }), "{err}");

    // Unknown workspace.
    let missing = knowell_store::WorkspaceId(uuid::Uuid::now_v7());
    assert!(matches!(
        create_project(&mut c, missing, src.id, &name("x"), None).await,
        Err(StoreError::NotFound {
            entity: "workspace",
            ..
        })
    ));

    // Lookups and listings are ordered and complete.
    assert_eq!(
        find_organization(&mut c, &name("acme")).await.unwrap(),
        Some(acme.clone())
    );
    assert_eq!(
        list_organizations(&mut c)
            .await
            .unwrap()
            .iter()
            .map(|o| o.name.to_string())
            .collect::<Vec<_>>(),
        ["acme", "globex"]
    );
    assert_eq!(
        find_workspace(&mut c, acme.id, &name("shop"))
            .await
            .unwrap(),
        Some(ws.clone())
    );
    assert_eq!(
        get_workspace(&mut c, ws.id).await.unwrap(),
        Some(ws.clone())
    );
    assert_eq!(
        list_workspaces(&mut c, acme.id).await.unwrap(),
        vec![ws.clone()]
    );
    assert_eq!(
        find_source(&mut c, acme.id, "/repos/shop").await.unwrap(),
        Some(src.clone())
    );
    assert_eq!(get_source(&mut c, src.id).await.unwrap(), Some(src.clone()));
    assert_eq!(
        list_sources(&mut c, acme.id).await.unwrap(),
        vec![src.clone()]
    );
    assert_eq!(
        list_projects(&mut c, ws.id).await.unwrap(),
        vec![api.clone(), web.clone()]
    );
    assert_eq!(
        find_project(&mut c, ws.id, &name("web")).await.unwrap(),
        Some(web.clone())
    );
    assert_eq!(
        get_project(&mut c, api.id).await.unwrap(),
        Some(api.clone())
    );

    // Renames respect uniqueness.
    assert!(matches!(
        rename_project(&mut c, web.id, &name("api")).await,
        Err(StoreError::AlreadyExists { .. })
    ));
    let renamed = rename_project(&mut c, web.id, &name("storefront"))
        .await
        .unwrap();
    assert_eq!(renamed.id, web.id);
    let ws2 = create_workspace(&mut c, acme.id, &name("ops"))
        .await
        .unwrap();
    assert!(matches!(
        rename_workspace(&mut c, ws2.id, &name("shop")).await,
        Err(StoreError::AlreadyExists { .. })
    ));
    assert_eq!(
        rename_workspace(&mut c, ws2.id, &name("platform"))
            .await
            .unwrap()
            .name,
        name("platform")
    );
}

#[tokio::test]
async fn views_are_unique_per_project_and_target() {
    let db = require_db!();
    let mut c = db.conn().await;
    let org = create_organization(&mut c, &name("acme")).await.unwrap();
    let ws = create_workspace(&mut c, org.id, &name("shop"))
        .await
        .unwrap();
    let src = create_source(&mut c, org.id, SourceKind::Git, "/repos/shop")
        .await
        .unwrap();
    let project = create_project(&mut c, ws.id, src.id, &name("api"), None)
        .await
        .unwrap();

    let main: TrackTarget = "branch:main".parse().unwrap();
    let release: TrackTarget = "tag:v2.1.0".parse().unwrap();
    let a = views::create_view(&mut c, project.id, &main).await.unwrap();
    assert_eq!(a.kind, knowell_store::ViewKind::Branch);
    assert_eq!(a.last_generation, 0);
    assert_eq!(a.active_generation, None);
    assert!(matches!(
        views::create_view(&mut c, project.id, &main).await,
        Err(StoreError::AlreadyExists { entity: "view", .. })
    ));
    let b = views::create_view(&mut c, project.id, &release)
        .await
        .unwrap();
    assert_eq!(b.kind, knowell_store::ViewKind::Tag);
    assert_eq!(
        views::find_view(&mut c, project.id, &release)
            .await
            .unwrap(),
        Some(b.clone())
    );
    assert_eq!(
        views::get_view(&mut c, a.id).await.unwrap(),
        Some(a.clone())
    );
    // Ordered by target text.
    assert_eq!(
        views::list_views(&mut c, project.id).await.unwrap(),
        vec![a, b]
    );
}

#[tokio::test]
async fn deletes_cascade_and_sources_in_use_are_protected() {
    let db = require_db!();
    let mut c = db.conn().await;
    let org = create_organization(&mut c, &name("acme")).await.unwrap();
    let ws = create_workspace(&mut c, org.id, &name("shop"))
        .await
        .unwrap();
    let src = create_source(&mut c, org.id, SourceKind::Git, "/repos/shop")
        .await
        .unwrap();
    let project = create_project(&mut c, ws.id, src.id, &name("api"), None)
        .await
        .unwrap();
    let main: TrackTarget = "branch:main".parse().unwrap();
    views::create_view(&mut c, project.id, &main).await.unwrap();

    let err = delete_source(&mut c, src.id).await.unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");

    assert!(delete_project(&mut c, project.id).await.unwrap());
    assert!(!delete_project(&mut c, project.id).await.unwrap());
    assert!(delete_source(&mut c, src.id).await.unwrap());

    // Deleting the organization removes everything below it.
    let src = create_source(&mut c, org.id, SourceKind::Git, "/repos/shop")
        .await
        .unwrap();
    create_project(&mut c, ws.id, src.id, &name("api"), None)
        .await
        .unwrap();
    assert!(delete_organization(&mut c, org.id).await.unwrap());
    assert_eq!(get_workspace(&mut c, ws.id).await.unwrap(), None);
    assert_eq!(get_source(&mut c, src.id).await.unwrap(), None);
    assert!(!delete_workspace(&mut c, ws.id).await.unwrap());
}

#[tokio::test]
async fn source_locations_with_credentials_are_rejected() {
    let db = require_db!();
    let mut c = db.conn().await;
    let org = create_organization(&mut c, &name("acme")).await.unwrap();
    let canary = "KNOWELL_CANARY_token_3e8b";
    let err = create_source(
        &mut c,
        org.id,
        SourceKind::Git,
        &format!("https://ci:{canary}@git.example.com/acme/shop.git"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)));
    assert!(!err.to_string().contains(canary));
    assert!(list_sources(&mut c, org.id).await.unwrap().is_empty());
}
