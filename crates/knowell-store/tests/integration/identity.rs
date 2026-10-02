use std::collections::BTreeSet;
use std::time::Duration;

use knowell_store::identity::*;
use knowell_store::{
    ApiTokenId, ApiTokenScope, GrantRole, PrincipalId, PrincipalKind, StoreError, hierarchy,
};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use crate::common::{Fixture, add_project, fixture, name, require_db};

const T0: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

fn user(principal_name: &str) -> NewPrincipal {
    NewPrincipal {
        id: None,
        kind: PrincipalKind::User,
        name: name(principal_name),
        display_name: None,
    }
}

fn token(principal: PrincipalId, prefix: &str, fill: u8) -> NewApiToken {
    NewApiToken {
        id: ApiTokenId(Uuid::now_v7()),
        principal,
        agent: None,
        prefix: prefix.into(),
        key_hash: [fill; 32],
        scopes: vec![ApiTokenScope::Read],
        label: Some("laptop".into()),
        created_by: None,
        created_at: T0,
        expires_at: None,
    }
}

async fn alice(c: &mut sqlx::PgConnection, fx: &Fixture) -> StoredPrincipal {
    create_principal(c, fx.org.id, &user("alice"))
        .await
        .unwrap()
}

#[tokio::test]
async fn principals_are_unique_per_organization_and_can_be_disabled() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let other = fixture(&mut c, "other").await;
    let fixed = PrincipalId(Uuid::from_u128(1));
    let a = create_principal(
        &mut c,
        fx.org.id,
        &NewPrincipal {
            id: Some(fixed),
            display_name: Some("Alice Example".into()),
            ..user("alice")
        },
    )
    .await
    .unwrap();
    assert_eq!(a.id, fixed);
    assert_eq!(a.kind, PrincipalKind::User);
    assert_eq!(a.display_name.as_deref(), Some("Alice Example"));
    let ci = create_principal(
        &mut c,
        fx.org.id,
        &NewPrincipal {
            kind: PrincipalKind::ServiceAccount,
            ..user("ci")
        },
    )
    .await
    .unwrap();
    // Same name in another organization is fine; twice in one is not.
    create_principal(&mut c, other.org.id, &user("alice"))
        .await
        .unwrap();
    assert!(matches!(
        create_principal(&mut c, fx.org.id, &user("alice")).await,
        Err(StoreError::AlreadyExists { .. })
    ));
    assert!(matches!(
        create_principal(
            &mut c,
            fx.org.id,
            &NewPrincipal {
                display_name: Some(String::new()),
                ..user("bob")
            }
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(get_principal(&mut c, a.id).await.unwrap(), Some(a.clone()));
    assert_eq!(
        find_principal(&mut c, fx.org.id, &name("ci"))
            .await
            .unwrap()
            .map(|p| p.id),
        Some(ci.id)
    );
    let names: Vec<String> = list_principals(&mut c, fx.org.id)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(names, ["alice", "ci"]);

    // Disabling keeps the earliest time; enabling clears it.
    let later = T0 + time::Duration::hours(1);
    let disabled = set_principal_disabled(&mut c, a.id, Some(T0))
        .await
        .unwrap();
    assert_eq!(disabled.disabled_at, Some(T0));
    let again = set_principal_disabled(&mut c, a.id, Some(later))
        .await
        .unwrap();
    assert_eq!(again.disabled_at, Some(T0));
    let enabled = set_principal_disabled(&mut c, a.id, None).await.unwrap();
    assert_eq!(enabled.disabled_at, None);
    assert!(matches!(
        set_principal_disabled(&mut c, PrincipalId(Uuid::now_v7()), None).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(delete_principal(&mut c, ci.id).await.unwrap());
    assert!(!delete_principal(&mut c, ci.id).await.unwrap());
}

#[tokio::test]
async fn grants_carry_current_names_and_respect_tenancy() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let (web, _) = add_project(&mut c, &fx, "web").await;
    let other = fixture(&mut c, "other").await;
    let a = alice(&mut c, &fx).await;
    let admin = create_principal(&mut c, fx.org.id, &user("root"))
        .await
        .unwrap();

    let org_grant = create_grant(
        &mut c,
        &NewGrant {
            principal: a.id,
            role: GrantRole::Viewer,
            scope: GrantScope::Organization,
            created_by: Some(admin.id),
        },
    )
    .await
    .unwrap();
    assert_eq!(org_grant.workspace_name, None);
    assert_eq!(org_grant.created_by, Some(admin.id));
    let ws_grant = create_grant(
        &mut c,
        &NewGrant {
            principal: a.id,
            role: GrantRole::Member,
            scope: GrantScope::Workspace(fx.workspace.id),
            created_by: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(ws_grant.workspace_name, Some(name("main")));
    let project_grant = create_grant(
        &mut c,
        &NewGrant {
            principal: a.id,
            role: GrantRole::Maintainer,
            scope: GrantScope::Project(web.id),
            created_by: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(project_grant.workspace_name, Some(name("main")));
    assert_eq!(project_grant.project_name, Some(name("web")));
    assert_eq!(project_grant.principal_kind, PrincipalKind::User);

    // Duplicates and foreign scopes are refused.
    assert!(matches!(
        create_grant(
            &mut c,
            &NewGrant {
                principal: a.id,
                role: GrantRole::Viewer,
                scope: GrantScope::Organization,
                created_by: None,
            }
        )
        .await,
        Err(StoreError::AlreadyExists { .. })
    ));
    for scope in [
        GrantScope::Workspace(other.workspace.id),
        GrantScope::Project(other.project.id),
    ] {
        assert!(
            matches!(
                create_grant(
                    &mut c,
                    &NewGrant {
                        principal: a.id,
                        role: GrantRole::Admin,
                        scope,
                        created_by: None,
                    }
                )
                .await,
                Err(StoreError::NotFound { .. })
            ),
            "{scope:?}"
        );
    }
    assert!(matches!(
        create_grant(
            &mut c,
            &NewGrant {
                principal: PrincipalId(Uuid::now_v7()),
                role: GrantRole::Admin,
                scope: GrantScope::Organization,
                created_by: None,
            }
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));

    // Ordered organization, workspace, project; names follow renames.
    hierarchy::rename_project(&mut c, web.id, &name("webapp"))
        .await
        .unwrap();
    let grants = grants_for_principal(&mut c, a.id).await.unwrap();
    assert_eq!(
        grants.iter().map(|g| g.id).collect::<Vec<_>>(),
        [org_grant.id, ws_grant.id, project_grant.id]
    );
    assert_eq!(grants[2].project_name, Some(name("webapp")));
    assert_eq!(list_grants(&mut c, fx.org.id, None).await.unwrap().len(), 3);
    assert!(
        list_grants(&mut c, other.org.id, None)
            .await
            .unwrap()
            .is_empty()
    );
    // A disabled principal holds no grants.
    set_principal_disabled(&mut c, a.id, Some(T0))
        .await
        .unwrap();
    assert!(grants_for_principal(&mut c, a.id).await.unwrap().is_empty());
    set_principal_disabled(&mut c, a.id, None).await.unwrap();
    // Deleting the project removes its grants.
    hierarchy::delete_project(&mut c, web.id).await.unwrap();
    assert_eq!(grants_for_principal(&mut c, a.id).await.unwrap().len(), 2);
    assert!(delete_grant(&mut c, ws_grant.id).await.unwrap());
    assert!(!delete_grant(&mut c, ws_grant.id).await.unwrap());
}

#[tokio::test]
async fn tokens_are_found_by_prefix_and_never_stored_in_plaintext() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let other = fixture(&mut c, "other").await;
    let a = alice(&mut c, &fx).await;
    let mallory = create_principal(&mut c, other.org.id, &user("mallory"))
        .await
        .unwrap();

    // Two records share a prefix (it is only 40 bits); both are candidates.
    let first = insert_api_token(&mut c, &token(a.id, "kn_abcdefgh", 1))
        .await
        .unwrap();
    let second = insert_api_token(
        &mut c,
        &NewApiToken {
            scopes: vec![ApiTokenScope::Write, ApiTokenScope::Read],
            expires_at: Some(T0 + time::Duration::days(30)),
            ..token(a.id, "kn_abcdefgh", 2)
        },
    )
    .await
    .unwrap();
    assert_eq!(second.scopes, [ApiTokenScope::Read, ApiTokenScope::Write]);
    // The same prefix in another organization is not a candidate.
    insert_api_token(&mut c, &token(mallory.id, "kn_abcdefgh", 3))
        .await
        .unwrap();
    let found = tokens_with_prefix(&mut c, fx.org.id, "kn_abcdefgh")
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|t| t.id).collect::<BTreeSet<_>>(),
        BTreeSet::from([first.id, second.id])
    );
    assert!(found.iter().all(|t| t.principal == a.id));
    assert_eq!(
        found.iter().find(|t| t.id == first.id).unwrap().key_hash,
        [1; 32]
    );
    for prefix in ["kn_zzzzzzzz", "kn_bad", "", "kn_ABCDEFGH"] {
        assert!(
            tokens_with_prefix(&mut c, fx.org.id, prefix)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        list_api_tokens(&mut c, fx.org.id, Some(a.id))
            .await
            .unwrap()
            .len(),
        2
    );

    // Duplicate hashes and malformed records are refused.
    assert!(matches!(
        insert_api_token(&mut c, &token(a.id, "kn_abcdefgh", 1)).await,
        Err(StoreError::AlreadyExists { .. })
    ));
    for bad in [
        token(a.id, "kn_ABCDEFGH", 9),
        NewApiToken {
            scopes: Vec::new(),
            ..token(a.id, "kn_abcdefgh", 9)
        },
        NewApiToken {
            expires_at: Some(T0),
            ..token(a.id, "kn_abcdefgh", 9)
        },
        NewApiToken {
            label: Some(String::new()),
            ..token(a.id, "kn_abcdefgh", 9)
        },
    ] {
        assert!(matches!(
            insert_api_token(&mut c, &bad).await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    assert!(matches!(
        insert_api_token(
            &mut c,
            &token(PrincipalId(Uuid::now_v7()), "kn_abcdefgh", 9)
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));

    // No column can hold a plaintext token.
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = 'api_token' ORDER BY ordinal_position",
    )
    .fetch_all(&mut *c)
    .await
    .unwrap();
    assert_eq!(
        columns,
        [
            "id",
            "organization_id",
            "principal_id",
            "agent_client",
            "agent_session",
            "prefix",
            "key_hash",
            "scopes",
            "label",
            "created_by",
            "created_at",
            "expires_at",
            "revoked_at",
            "last_used_at"
        ]
    );
}

#[tokio::test]
async fn agent_tokens_follow_the_agent_rules() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let a = alice(&mut c, &fx).await;
    let ci = create_principal(
        &mut c,
        fx.org.id,
        &NewPrincipal {
            kind: PrincipalKind::ServiceAccount,
            ..user("ci")
        },
    )
    .await
    .unwrap();
    let agent = TokenAgent {
        client: name("test-agent"),
        session: Uuid::from_u128(77),
    };
    let ok = insert_api_token(
        &mut c,
        &NewApiToken {
            agent: Some(agent.clone()),
            scopes: vec![ApiTokenScope::Read, ApiTokenScope::Write],
            expires_at: Some(T0 + time::Duration::hours(1)),
            ..token(a.id, "kn_agentaaa", 1)
        },
    )
    .await
    .unwrap();
    assert_eq!(ok.agent, Some(agent.clone()));
    for bad in [
        // Service accounts have no agents.
        NewApiToken {
            agent: Some(agent.clone()),
            expires_at: Some(T0 + time::Duration::hours(1)),
            ..token(ci.id, "kn_agentaaa", 2)
        },
        // Agent tokens must expire, within 24 hours, without admin.
        NewApiToken {
            agent: Some(agent.clone()),
            ..token(a.id, "kn_agentaaa", 3)
        },
        NewApiToken {
            agent: Some(agent.clone()),
            expires_at: Some(T0 + time::Duration::hours(25)),
            ..token(a.id, "kn_agentaaa", 4)
        },
        NewApiToken {
            agent: Some(agent.clone()),
            scopes: vec![ApiTokenScope::Admin],
            expires_at: Some(T0 + time::Duration::hours(1)),
            ..token(a.id, "kn_agentaaa", 5)
        },
    ] {
        let err = insert_api_token(&mut c, &bad).await.unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    }
}

#[tokio::test]
async fn revocation_disable_and_throttled_last_use() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let a = alice(&mut c, &fx).await;
    let stored = insert_api_token(&mut c, &token(a.id, "kn_abcdefgh", 1))
        .await
        .unwrap();
    assert_eq!(stored.revoked_at, None);
    assert_eq!(stored.last_used_at, None);

    // last_used_at: written once per interval.
    let minute = Duration::from_secs(60);
    let at = T0 + time::Duration::hours(1);
    assert!(
        touch_api_token(&mut c, stored.id, at, minute)
            .await
            .unwrap()
    );
    assert!(
        !touch_api_token(&mut c, stored.id, at + time::Duration::seconds(30), minute)
            .await
            .unwrap()
    );
    let later = at + time::Duration::seconds(61);
    assert!(
        touch_api_token(&mut c, stored.id, later, minute)
            .await
            .unwrap()
    );
    assert_eq!(
        get_api_token(&mut c, stored.id)
            .await
            .unwrap()
            .unwrap()
            .last_used_at,
        Some(later)
    );
    assert!(
        !touch_api_token(&mut c, ApiTokenId(Uuid::now_v7()), later, minute)
            .await
            .unwrap()
    );

    // Revocation is idempotent and keeps the earliest time.
    let r1 = T0 + time::Duration::hours(2);
    assert!(revoke_api_token(&mut c, stored.id, r1).await.unwrap());
    assert!(
        revoke_api_token(&mut c, stored.id, r1 + time::Duration::hours(1))
            .await
            .unwrap()
    );
    let revoked = get_api_token(&mut c, stored.id).await.unwrap().unwrap();
    assert_eq!(revoked.revoked_at, Some(r1));
    assert_eq!(revoked.effective_revoked_at(), Some(r1));
    assert!(
        !revoke_api_token(&mut c, ApiTokenId(Uuid::now_v7()), r1)
            .await
            .unwrap()
    );

    // A disabled principal's tokens read as revoked from the disable time.
    let other = insert_api_token(&mut c, &token(a.id, "kn_abcdefgh", 2))
        .await
        .unwrap();
    set_principal_disabled(&mut c, a.id, Some(T0))
        .await
        .unwrap();
    let candidates = tokens_with_prefix(&mut c, fx.org.id, "kn_abcdefgh")
        .await
        .unwrap();
    for t in &candidates {
        assert_eq!(t.principal_disabled_at, Some(T0));
        assert_eq!(t.effective_revoked_at(), Some(T0));
    }
    assert!(delete_api_token(&mut c, other.id).await.unwrap());
    // Deleting the principal deletes its tokens.
    delete_principal(&mut c, a.id).await.unwrap();
    assert_eq!(get_api_token(&mut c, stored.id).await.unwrap(), None);
}
