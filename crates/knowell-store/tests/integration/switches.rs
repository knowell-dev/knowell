use knowell_store::embeddings::{
    EmbeddingProfile, NewEmbeddingProfile, activate_index_generation, begin_index_generation,
    register_profile,
};
use knowell_store::switches::*;
use knowell_store::views::{self, GenerationPin};
use knowell_store::{OrganizationId, ProfileSwitchState, StoreError, ViewId, hierarchy};
use uuid::Uuid;

use crate::common::{Fixture, add_project, fixture, name, require_db};

async fn profile(
    c: &mut sqlx::PgConnection,
    org: OrganizationId,
    label: &str,
    dims: u32,
) -> EmbeddingProfile {
    register_profile(
        c,
        org,
        &NewEmbeddingProfile {
            name: name(label),
            provider: "test".into(),
            model: "m1".into(),
            dimensions: dims,
            input_format_version: "title-text-v1".into(),
        },
    )
    .await
    .unwrap()
}

/// Builds and activates the next generation of `view`.
async fn next_generation(c: &mut sqlx::PgConnection, view: ViewId) -> i64 {
    let generation = views::begin_generation(c, view, None).await.unwrap();
    views::activate_generation(c, view, generation)
        .await
        .unwrap();
    generation
}

/// Activates the vector index generation of `profile` for `generation`.
async fn cover(
    c: &mut sqlx::PgConnection,
    view: ViewId,
    generation: i64,
    profile: &EmbeddingProfile,
) {
    let pin = GenerationPin { view, generation };
    let index = begin_index_generation(c, pin, profile.id).await.unwrap();
    activate_index_generation(c, index.id).await.unwrap();
}

fn switch(
    fx: &Fixture,
    from: Option<&EmbeddingProfile>,
    to: &EmbeddingProfile,
    views: Vec<ViewId>,
) -> NewSwitch {
    NewSwitch {
        organization: fx.org.id,
        workspace: fx.workspace.id,
        from: from.map(|p| p.id),
        to: to.id,
        views,
        origin: SwitchOrigin::Request,
        requested_by: "user:alice".into(),
        retention_seconds: 3600,
    }
}

async fn serving(
    c: &mut sqlx::PgConnection,
    views: &[ViewId],
) -> Vec<Option<knowell_store::ProfileId>> {
    let rows = view_embeddings(c, views).await.unwrap();
    views
        .iter()
        .map(|v| rows.get(v).and_then(|row| row.serving))
        .collect()
}

#[tokio::test]
async fn configured_profiles_bootstrap_serving_and_report_changes() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let (_, unembedded) = add_project(&mut c, &fx, "docs").await;
    let old = profile(&mut c, fx.org.id, "old", 4).await;
    let new = profile(&mut c, fx.org.id, "new", 8).await;
    let view = fx.view.id;

    let ConfiguredProfile::Recorded(first) = record_configured_profile(&mut c, view, Some(old.id))
        .await
        .unwrap()
    else {
        panic!("the first registration records the configured profile");
    };
    assert_eq!(
        (first.serving, first.configured),
        (Some(old.id), Some(old.id))
    );
    assert!(matches!(
        record_configured_profile(&mut c, view, Some(old.id)).await.unwrap(),
        ConfiguredProfile::Unchanged(row) if row == first
    ));
    // A changed configuration is reported and changes nothing until it is
    // acknowledged; even then the old profile keeps serving.
    assert!(matches!(
        record_configured_profile(&mut c, view, Some(new.id)).await.unwrap(),
        ConfiguredProfile::Differs(row) if row == first
    ));
    assert!(matches!(
        record_configured_profile(&mut c, view, Some(new.id))
            .await
            .unwrap(),
        ConfiguredProfile::Differs(_)
    ));
    let acknowledged = acknowledge_configured_profile(&mut c, view, Some(new.id))
        .await
        .unwrap();
    assert_eq!(
        (acknowledged.serving, acknowledged.configured),
        (Some(old.id), Some(new.id))
    );
    assert!(matches!(
        record_configured_profile(&mut c, view, Some(new.id))
            .await
            .unwrap(),
        ConfiguredProfile::Unchanged(_)
    ));

    // A view that embedded with nothing serves its first configured profile.
    assert!(matches!(
        record_configured_profile(&mut c, unembedded.id, None)
            .await
            .unwrap(),
        ConfiguredProfile::Recorded(ViewEmbedding { serving: None, .. })
    ));
    assert!(matches!(
        record_configured_profile(&mut c, unembedded.id, Some(new.id))
            .await
            .unwrap(),
        ConfiguredProfile::Differs(ViewEmbedding { serving: None, .. })
    ));
    let now = acknowledge_configured_profile(&mut c, unembedded.id, Some(new.id))
        .await
        .unwrap();
    assert_eq!(now.serving, Some(new.id));
    assert_eq!(
        serving(&mut c, &[view, unembedded.id, ViewId(Uuid::now_v7())]).await,
        [Some(old.id), Some(new.id), None]
    );
    assert!(matches!(
        record_configured_profile(&mut c, ViewId(Uuid::now_v7()), Some(old.id)).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        acknowledge_configured_profile(&mut c, ViewId(Uuid::now_v7()), Some(old.id)).await,
        Err(StoreError::NotFound { .. })
    ));
}

#[tokio::test]
async fn switches_move_every_view_at_once_only_when_covered() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let (_, web) = add_project(&mut c, &fx, "web").await;
    let (_, empty) = add_project(&mut c, &fx, "empty").await;
    let old = profile(&mut c, fx.org.id, "old", 4).await;
    let new = profile(&mut c, fx.org.id, "new", 8).await;
    let views = vec![fx.view.id, web.id, empty.id];
    for view in &views {
        record_configured_profile(&mut c, *view, Some(old.id))
            .await
            .unwrap();
    }
    let api_g1 = next_generation(&mut c, fx.view.id).await;
    let web_g1 = next_generation(&mut c, web.id).await;
    cover(&mut c, fx.view.id, api_g1, &old).await;
    cover(&mut c, web.id, web_g1, &old).await;

    let started = start_switch(&mut c, &switch(&fx, Some(&old), &new, views.clone()))
        .await
        .unwrap();
    assert_eq!(started.state, ProfileSwitchState::Building);
    let mut sorted = views.clone();
    sorted.sort();
    assert_eq!(started.views, sorted);
    assert_eq!(
        building_switch_of_view(&mut c, web.id).await.unwrap(),
        Some(started.clone())
    );
    assert!(matches!(
        start_switch(&mut c, &switch(&fx, Some(&old), &new, vec![web.id])).await,
        Err(StoreError::AlreadyExists { .. })
    ));

    // Nothing moves until the target covers every active generation; a view
    // without an active generation does not hold the switch back.
    let mut both = vec![fx.view.id, web.id];
    both.sort();
    assert_eq!(
        activate_switch(&mut c, fx.org.id, started.id)
            .await
            .unwrap(),
        SwitchActivation::Pending(both)
    );
    cover(&mut c, fx.view.id, api_g1, &new).await;
    assert_eq!(
        activate_switch(&mut c, fx.org.id, started.id)
            .await
            .unwrap(),
        SwitchActivation::Pending(vec![web.id])
    );
    assert_eq!(
        serving(&mut c, &views).await,
        [Some(old.id), Some(old.id), Some(old.id)]
    );
    cover(&mut c, web.id, web_g1, &new).await;
    let SwitchActivation::Activated(active) = activate_switch(&mut c, fx.org.id, started.id)
        .await
        .unwrap()
    else {
        panic!("a covered switch activates");
    };
    assert_eq!(active.state, ProfileSwitchState::Active);
    let (activated, until) = (
        active.activated_at.unwrap(),
        active.reversible_until.unwrap(),
    );
    assert_eq!(until - activated, time::Duration::seconds(3600));
    assert_eq!(
        serving(&mut c, &views).await,
        [Some(new.id), Some(new.id), Some(new.id)]
    );
    assert!(
        view_embeddings(&mut c, &views)
            .await
            .unwrap()
            .values()
            .all(|row| row.switch == Some(started.id) && row.configured == Some(old.id))
    );
    assert!(matches!(
        activate_switch(&mut c, fx.org.id, started.id).await.unwrap(),
        SwitchActivation::NotBuilding(s) if s.state == ProfileSwitchState::Active
    ));
    assert_eq!(building_switch_of_view(&mut c, web.id).await.unwrap(), None);
}

#[tokio::test]
async fn rollbacks_cancellations_and_their_limits() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let old = profile(&mut c, fx.org.id, "old", 4).await;
    let new = profile(&mut c, fx.org.id, "new", 8).await;
    let view = fx.view.id;
    record_configured_profile(&mut c, view, Some(old.id))
        .await
        .unwrap();
    let g1 = next_generation(&mut c, view).await;
    cover(&mut c, view, g1, &old).await;
    cover(&mut c, view, g1, &new).await;

    // A cancelled switch leaves the old profile serving.
    let cancelled = start_switch(&mut c, &switch(&fx, Some(&old), &new, vec![view]))
        .await
        .unwrap();
    let after = cancel_switch(&mut c, fx.org.id, cancelled.id)
        .await
        .unwrap();
    assert_eq!(after.state, ProfileSwitchState::Cancelled);
    assert!(after.finished_at.is_some());
    assert!(matches!(
        cancel_switch(&mut c, fx.org.id, cancelled.id).await,
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(serving(&mut c, &[view]).await, [Some(old.id)]);

    // Activate, then roll back: the old vectors still cover the generation,
    // so the reverse switch activates at once.
    let forward = start_switch(&mut c, &switch(&fx, Some(&old), &new, vec![view]))
        .await
        .unwrap();
    assert!(matches!(
        activate_switch(&mut c, fx.org.id, forward.id)
            .await
            .unwrap(),
        SwitchActivation::Activated(_)
    ));
    let reverse = start_rollback(&mut c, fx.org.id, forward.id, "user:alice")
        .await
        .unwrap();
    assert_eq!(
        (
            reverse.from,
            reverse.to,
            reverse.origin,
            reverse.rollback_of
        ),
        (
            Some(new.id),
            old.id,
            SwitchOrigin::Rollback,
            Some(forward.id)
        )
    );
    assert!(matches!(
        activate_switch(&mut c, fx.org.id, reverse.id)
            .await
            .unwrap(),
        SwitchActivation::Activated(_)
    ));
    assert_eq!(serving(&mut c, &[view]).await, [Some(old.id)]);
    assert_eq!(
        get_switch(&mut c, fx.org.id, forward.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ProfileSwitchState::RolledBack
    );
    // A rolled-back switch cannot be rolled back again.
    assert!(matches!(
        start_rollback(&mut c, fx.org.id, forward.id, "user:alice").await,
        Err(StoreError::InvalidInput(_))
    ));

    // No rollback after the window, nor once another switch moved the views.
    let closed = start_switch(
        &mut c,
        &NewSwitch {
            retention_seconds: 0,
            ..switch(&fx, Some(&old), &new, vec![view])
        },
    )
    .await
    .unwrap();
    activate_switch(&mut c, fx.org.id, closed.id).await.unwrap();
    let err = start_rollback(&mut c, fx.org.id, closed.id, "user:alice")
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    let moved = start_switch(&mut c, &switch(&fx, Some(&new), &old, vec![view]))
        .await
        .unwrap();
    activate_switch(&mut c, fx.org.id, moved.id).await.unwrap();
    let back = start_switch(&mut c, &switch(&fx, Some(&old), &new, vec![view]))
        .await
        .unwrap();
    activate_switch(&mut c, fx.org.id, back.id).await.unwrap();
    assert!(matches!(
        start_rollback(&mut c, fx.org.id, moved.id, "user:alice").await,
        Err(StoreError::InvalidInput(_))
    ));
    // A switch that replaced no profile has nothing to roll back to.
    let (_, fresh) = add_project(&mut c, &fx, "fresh").await;
    let first = start_switch(&mut c, &switch(&fx, None, &new, vec![fresh.id]))
        .await
        .unwrap();
    activate_switch(&mut c, fx.org.id, first.id).await.unwrap();
    assert!(matches!(
        start_rollback(&mut c, fx.org.id, first.id, "user:alice").await,
        Err(StoreError::InvalidInput(_))
    ));
    let listed = list_switches(&mut c, fx.org.id, 100).await.unwrap();
    assert_eq!(listed.len(), 7);
    assert!(
        listed
            .windows(2)
            .all(|w| w[0].created_at >= w[1].created_at)
    );
}

#[tokio::test]
async fn switches_are_validated_scoped_and_cascade() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let other = fixture(&mut c, "other").await;
    let old = profile(&mut c, fx.org.id, "old", 4).await;
    let new = profile(&mut c, fx.org.id, "new", 8).await;
    let foreign = profile(&mut c, other.org.id, "foreign", 8).await;
    let canary = "KNOWELL_CANARY_requester";
    let base = switch(&fx, Some(&old), &new, vec![fx.view.id]);

    for (bad, expect_not_found) in [
        (
            NewSwitch {
                views: vec![other.view.id],
                ..base.clone()
            },
            true,
        ),
        (
            NewSwitch {
                to: foreign.id,
                ..base.clone()
            },
            true,
        ),
        (
            NewSwitch {
                workspace: other.workspace.id,
                ..base.clone()
            },
            true,
        ),
        (
            NewSwitch {
                views: Vec::new(),
                ..base.clone()
            },
            false,
        ),
        (
            NewSwitch {
                to: old.id,
                ..base.clone()
            },
            false,
        ),
        (
            NewSwitch {
                requested_by: format!("{canary} x"),
                ..base.clone()
            },
            false,
        ),
        (
            NewSwitch {
                origin: SwitchOrigin::Rollback,
                ..base.clone()
            },
            false,
        ),
    ] {
        let err = start_switch(&mut c, &bad).await.unwrap_err();
        if expect_not_found {
            assert!(matches!(err, StoreError::NotFound { .. }), "{err}");
        } else {
            assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
        }
        assert!(!err.to_string().contains(canary), "{err}");
    }
    assert!(
        building_switches(&mut c, fx.org.id)
            .await
            .unwrap()
            .is_empty()
    );

    let started = start_switch(&mut c, &base).await.unwrap();
    assert_eq!(
        building_switches(&mut c, fx.org.id).await.unwrap(),
        std::slice::from_ref(&started)
    );
    // Another organization sees nothing of it.
    assert_eq!(
        get_switch(&mut c, other.org.id, started.id).await.unwrap(),
        None
    );
    assert!(
        building_switches(&mut c, other.org.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cancel_switch(&mut c, other.org.id, started.id).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        activate_switch(&mut c, other.org.id, started.id).await,
        Err(StoreError::NotFound { .. })
    ));
    // Deleting the organization removes its switches and serving rows.
    record_configured_profile(&mut c, fx.view.id, Some(old.id))
        .await
        .unwrap();
    hierarchy::delete_organization(&mut c, fx.org.id)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM profile_switch) + (SELECT count(*) FROM profile_switch_view)
              + (SELECT count(*) FROM view_embedding)",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(left, 0);
}
