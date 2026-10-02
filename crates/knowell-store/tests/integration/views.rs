use knowell_core::ContentHash;
use knowell_store::content::{self, FileChange};
use knowell_store::views::*;
use knowell_store::{GenerationState, StoreError};

use crate::common::{add_project, commit, fixture, name, path, require_db};

#[tokio::test]
async fn activation_is_fenced_against_older_generations() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;

    let g1 = begin_generation(&mut c, view, Some(&commit(1)))
        .await
        .unwrap();
    assert_eq!(g1, 1);
    // Only one generation builds at a time.
    assert!(matches!(
        begin_generation(&mut c, view, Some(&commit(2))).await,
        Err(StoreError::GenerationBusy { building: 1, .. })
    ));
    assert_eq!(building_generation(&mut c, view).await.unwrap(), Some(1));
    let a1 = activate_generation(&mut c, view, g1).await.unwrap();
    assert_eq!(a1.previous, None);

    let g2 = begin_generation(&mut c, view, Some(&commit(2)))
        .await
        .unwrap();
    assert_eq!(g2, 2);
    let a2 = activate_generation(&mut c, view, g2).await.unwrap();
    assert_eq!(a2.previous, Some(1));

    // A late job of generation 1 can never activate over generation 2.
    match activate_generation(&mut c, view, g1).await {
        Err(StoreError::StaleGeneration {
            generation: 1,
            active: 2,
            ..
        }) => {}
        other => panic!("expected stale generation, got {other:?}"),
    }
    // Re-activating the active generation is stale too.
    assert!(matches!(
        activate_generation(&mut c, view, g2).await,
        Err(StoreError::StaleGeneration { active: 2, .. })
    ));

    // The zombie scenario: generation 3 is abandoned (its worker's lease
    // expired), generation 4 takes over and activates, then the zombie
    // finishes and tries to write and activate.
    let g3 = begin_generation(&mut c, view, Some(&commit(3)))
        .await
        .unwrap();
    fail_generation(&mut c, view, g3, "lease expired")
        .await
        .unwrap();
    let g4 = begin_generation(&mut c, view, Some(&commit(4)))
        .await
        .unwrap();
    activate_generation(&mut c, view, g4).await.unwrap();
    let late_write = content::apply_file_changes(
        &mut c,
        view,
        g3,
        &[FileChange::Delete { path: path("a.rs") }],
    )
    .await;
    assert!(matches!(
        late_write,
        Err(StoreError::GenerationNotBuilding {
            state: GenerationState::Failed,
            ..
        })
    ));
    assert!(matches!(
        activate_generation(&mut c, view, g3).await,
        Err(StoreError::StaleGeneration { active: 4, .. })
    ));

    // A failed generation newer than the active one is "not building".
    let g5 = begin_generation(&mut c, view, None).await.unwrap();
    fail_generation(&mut c, view, g5, "parser crashed")
        .await
        .unwrap();
    assert!(matches!(
        activate_generation(&mut c, view, g5).await,
        Err(StoreError::GenerationNotBuilding {
            state: GenerationState::Failed,
            ..
        })
    ));
    assert!(matches!(
        fail_generation(&mut c, view, g5, "again").await,
        Err(StoreError::GenerationNotBuilding { .. })
    ));
    assert!(matches!(
        activate_generation(&mut c, view, 99).await,
        Err(StoreError::NotFound { .. })
    ));

    let v = get_view(&mut c, view).await.unwrap().unwrap();
    assert_eq!(v.active_generation, Some(4));
    assert_eq!(v.active_commit, Some(commit(4)));
    assert_eq!(v.last_generation, 5);
    let states: Vec<(i64, GenerationState)> = list_generations(&mut c, view)
        .await
        .unwrap()
        .iter()
        .map(|g| (g.generation, g.state))
        .collect();
    assert_eq!(
        states,
        [
            (5, GenerationState::Failed),
            (4, GenerationState::Active),
            (3, GenerationState::Failed),
            (2, GenerationState::Retired),
            (1, GenerationState::Retired),
        ]
    );
    let g3row = get_generation(&mut c, view, 3).await.unwrap().unwrap();
    assert_eq!(g3row.error.as_deref(), Some("lease expired"));
    assert!(g3row.finished_at.is_some());
}

#[tokio::test]
async fn concurrent_activations_never_go_backwards() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    // Race a late activation of an old generation against many fresh
    // begin/activate cycles; the active generation must only ever grow.
    let g1 = begin_generation(&mut c, view, None).await.unwrap();
    activate_generation(&mut c, view, g1).await.unwrap();
    let store = db.store.clone();
    let racer = tokio::spawn(async move {
        let mut c = store.acquire().await.unwrap();
        let mut stale = 0;
        for _ in 0..20 {
            if let Err(StoreError::StaleGeneration { .. }) =
                activate_generation(&mut c, view, g1).await
            {
                stale += 1;
            }
        }
        stale
    });
    let mut last = g1;
    for _ in 0..20 {
        let g = begin_generation(&mut c, view, None).await.unwrap();
        let a = activate_generation(&mut c, view, g).await.unwrap();
        assert_eq!(a.previous, Some(last));
        last = g;
    }
    assert_eq!(racer.await.unwrap(), 20);
    let v = get_view(&mut c, view).await.unwrap().unwrap();
    assert_eq!(v.active_generation, Some(last));
}

#[tokio::test]
async fn busy_begin_keeps_the_callers_transaction_usable() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    begin_generation(&mut c, fx.view.id, None).await.unwrap();
    let mut tx = db.store.begin().await.unwrap();
    assert!(matches!(
        begin_generation(&mut tx, fx.view.id, None).await,
        Err(StoreError::GenerationBusy { .. })
    ));
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(one, 1);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn failing_a_generation_rolls_its_rows_back() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let h = |s: &str| ContentHash::of(s.as_bytes());

    let g1 = begin_generation(&mut c, view, None).await.unwrap();
    content::apply_file_changes(
        &mut c,
        view,
        g1,
        &[
            FileChange::Upsert {
                path: path("a.rs"),
                content_hash: h("a1"),
                renamed_from: None,
            },
            FileChange::Upsert {
                path: path("b.rs"),
                content_hash: h("b1"),
                renamed_from: None,
            },
        ],
    )
    .await
    .unwrap();
    activate_generation(&mut c, view, g1).await.unwrap();

    let g2 = begin_generation(&mut c, view, None).await.unwrap();
    content::apply_file_changes(
        &mut c,
        view,
        g2,
        &[
            FileChange::Upsert {
                path: path("a.rs"),
                content_hash: h("a2"),
                renamed_from: None,
            },
            FileChange::Delete { path: path("b.rs") },
            FileChange::Upsert {
                path: path("c.rs"),
                content_hash: h("c2"),
                renamed_from: None,
            },
        ],
    )
    .await
    .unwrap();
    // Readers of the active generation do not see building rows.
    let at1 = |files: Vec<content::FileVersion>| {
        files
            .into_iter()
            .map(|f| (f.path.to_string(), f.content_hash))
            .collect::<Vec<_>>()
    };
    let pin1 = GenerationPin {
        view,
        generation: 1,
    };
    let before = at1(content::files_at(&mut c, pin1).await.unwrap());
    assert_eq!(before, [("a.rs".into(), h("a1")), ("b.rs".into(), h("b1"))]);

    fail_generation(&mut c, view, g2, "boom").await.unwrap();
    assert_eq!(at1(content::files_at(&mut c, pin1).await.unwrap()), before);

    // The next generation starts from the active data.
    let g3 = begin_generation(&mut c, view, None).await.unwrap();
    let pin3 = GenerationPin {
        view,
        generation: g3,
    };
    assert_eq!(at1(content::files_at(&mut c, pin3).await.unwrap()), before);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM file_version WHERE view_id = $1 AND valid_to IS NULL",
    )
    .bind(view)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(open, 2);
}

#[tokio::test]
async fn manifests_pin_a_consistent_multi_project_snapshot() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let (web, web_view) = add_project(&mut c, &fx, "web").await;

    for view in [fx.view.id, web_view.id] {
        let g = begin_generation(&mut c, view, Some(&commit(1)))
            .await
            .unwrap();
        activate_generation(&mut c, view, g).await.unwrap();
    }
    // A view of the same project cannot be pinned twice.
    let tag: knowell_core::TrackTarget = "tag:v1.0.0".parse().unwrap();
    let api_tag = create_view(&mut c, fx.project.id, &tag).await.unwrap();

    // Without an active generation, a view cannot be pinned (no fallback).
    let err = active_pins(&mut c, &[api_tag.id]).await.unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    let err = pin_manifest(&mut c, fx.workspace.id, None, &[fx.view.id, api_tag.id])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    let g = begin_generation(&mut c, api_tag.id, None).await.unwrap();
    activate_generation(&mut c, api_tag.id, g).await.unwrap();
    let err = pin_manifest(&mut c, fx.workspace.id, None, &[fx.view.id, api_tag.id])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");

    let release = pin_manifest(
        &mut c,
        fx.workspace.id,
        Some(&name("release-1")),
        &[fx.view.id, web_view.id],
    )
    .await
    .unwrap();
    assert_eq!(release.entries.len(), 2);
    assert!(release.entries.iter().all(|e| e.generation == 1));
    assert!(
        release
            .entries
            .iter()
            .all(|e| e.resolved_commit.as_deref() == Some(commit(1).as_str()))
    );
    assert!(release.entries.iter().any(|e| e.project == web.id));

    // A newer generation does not change the pinned snapshot.
    let g2 = begin_generation(&mut c, fx.view.id, Some(&commit(2)))
        .await
        .unwrap();
    activate_generation(&mut c, fx.view.id, g2).await.unwrap();
    let again = find_manifest(&mut c, fx.workspace.id, &name("release-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again, release);
    assert_eq!(
        get_manifest(&mut c, release.id).await.unwrap(),
        Some(release.clone())
    );
    let pins = active_pins(&mut c, &[fx.view.id, web_view.id])
        .await
        .unwrap();
    assert!(pins.contains(&GenerationPin {
        view: fx.view.id,
        generation: 2
    }));

    assert!(matches!(
        pin_manifest(
            &mut c,
            fx.workspace.id,
            Some(&name("release-1")),
            &[web_view.id]
        )
        .await,
        Err(StoreError::AlreadyExists { .. })
    ));

    // A view of another workspace is rejected.
    let other_ws = knowell_store::hierarchy::create_workspace(&mut c, fx.org.id, &name("other"))
        .await
        .unwrap();
    let err = pin_manifest(&mut c, other_ws.id, None, &[fx.view.id])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");

    // The pinned generation is protected while the manifest exists.
    let err = delete_view(&mut c, fx.view.id).await.unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    assert!(delete_manifest(&mut c, release.id).await.unwrap());
    assert!(delete_view(&mut c, fx.view.id).await.unwrap());
}

#[tokio::test]
async fn prune_keeps_everything_a_pin_or_the_active_generation_can_see() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let h = |n: u32| ContentHash::of(&n.to_le_bytes());
    // Generations 1..=4 each rewrite a.rs.
    for n in 1..=4u32 {
        let g = begin_generation(&mut c, view, None).await.unwrap();
        content::apply_file_changes(
            &mut c,
            view,
            g,
            &[FileChange::Upsert {
                path: path("a.rs"),
                content_hash: h(n),
                renamed_from: None,
            }],
        )
        .await
        .unwrap();
        activate_generation(&mut c, view, g).await.unwrap();
        if n == 2 {
            pin_manifest(&mut c, fx.workspace.id, Some(&name("v2")), &[view])
                .await
                .unwrap();
        }
    }
    // The pin on generation 2 lowers the cutoff from 4 to 2.
    let summary = prune_history(&mut c, view, 4).await.unwrap();
    assert_eq!(summary.cutoff, 2);
    assert_eq!(summary.rows_deleted, 1); // generation 1's version of a.rs
    assert_eq!(summary.generations_deleted, 1);
    let at = |g| GenerationPin {
        view,
        generation: g,
    };
    assert_eq!(
        content::files_at(&mut c, at(2)).await.unwrap()[0].content_hash,
        h(2)
    );
    assert_eq!(
        content::files_at(&mut c, at(4)).await.unwrap()[0].content_hash,
        h(4)
    );
    assert!(content::files_at(&mut c, at(1)).await.unwrap().is_empty());

    let manifest = find_manifest(&mut c, fx.workspace.id, &name("v2"))
        .await
        .unwrap()
        .unwrap();
    delete_manifest(&mut c, manifest.id).await.unwrap();
    let summary = prune_history(&mut c, view, 100).await.unwrap();
    assert_eq!(summary.cutoff, 4);
    assert_eq!(summary.rows_deleted, 2);
    assert_eq!(
        content::files_at(&mut c, at(4)).await.unwrap()[0].content_hash,
        h(4)
    );
    assert_eq!(
        list_generations(&mut c, view).await.unwrap().len(),
        1,
        "only the active generation remains"
    );
}

#[tokio::test]
async fn seen_commits_are_recorded_and_validated() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    record_seen_commit(&mut c, fx.view.id, &commit(9))
        .await
        .unwrap();
    let v = get_view(&mut c, fx.view.id).await.unwrap().unwrap();
    assert_eq!(v.latest_seen_commit, Some(commit(9)));
    assert!(matches!(
        record_seen_commit(&mut c, fx.view.id, "main").await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        begin_generation(&mut c, fx.view.id, Some("HEAD")).await,
        Err(StoreError::InvalidInput(_))
    ));
}
