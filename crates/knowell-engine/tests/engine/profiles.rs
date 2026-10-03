//! Persisted profile metadata has an organization gate and needs no source or provider.

use std::sync::Arc;

use knowell_auth::{
    Grant, GrantSet, Principal, ResourceScope, Role, TokenScope, TokenScopes, UserId,
};
use knowell_engine::{
    Access, AccessResolver, BoxFuture, Engine, ProfileMetadata, ProfileSelector, StaticAccess,
    StoreAccess,
};
use knowell_mcp::{Caller, ClientIdentity, Timestamp, ToolError, TransportKind};
use knowell_store::identity::{self, GrantScope, NewGrant, NewPrincipal};
use knowell_store::{
    GrantRole, OrganizationId, PrincipalId, PrincipalKind, SourceKind, Store, hierarchy,
};
use serde_json::Value;
use uuid::Uuid;

use crate::common::{
    PLAIN_ENV, TestDb, alice, alice_caller, engine_config, indexer_config, name, require_db,
};

const CREATED: &str = "2021-02-03T04:05:06.123456Z";
const MODEL: &str = "Synthetic ![model](https://example.invalid) <script> ```\u{001b}[31m\t";

/// Direct metadata inserts deliberately create no vector index. Catalogue
/// reads must work even when providers were removed or pgvector is absent.
async fn seed(store: &Store, organization: OrganizationId) -> Vec<ProfileMetadata> {
    let mut conn = store.acquire().await.unwrap();
    let mut expected = Vec::new();
    for (ordinal, label) in [(3, "z-profile"), (2, "a_profile"), (1, "a-profile")] {
        let id = Uuid::from_u128(0xCA7A_1000 + ordinal);
        let model = if ordinal == 3 {
            MODEL
        } else {
            "synthetic-unavailable-model"
        };
        let dimensions = 64 + u32::try_from(ordinal).unwrap();
        sqlx::query(
            "INSERT INTO embedding_profile
             (id, organization_id, name, provider, model, dimensions, input_format_version, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::timestamptz)",
        )
        .bind(id)
        .bind(organization)
        .bind(label)
        .bind("removed-synthetic-provider")
        .bind(model)
        .bind(i32::try_from(dimensions).unwrap())
        .bind("synthetic-input-v7")
        .bind(CREATED)
        .execute(&mut *conn)
        .await
        .unwrap();
        expected.push(ProfileMetadata {
            id,
            name: name(label),
            provider: "removed-synthetic-provider".to_owned(),
            model: model.to_owned(),
            dimensions,
            input_format_version: "synthetic-input-v7".to_owned(),
            created_at: Timestamp::new(CREATED).unwrap(),
        });
    }
    expected.sort_by(|a, b| a.name.cmp(&b.name));
    expected
}

async fn snapshot(store: &Store) -> Value {
    let mut conn = store.acquire().await.unwrap();
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
         'profiles', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM embedding_profile t),
         'jobs', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM job t),
         'views', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM view t),
         'generations', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM view_generation t),
         'sources', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM source t),
         'workspaces', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM workspace t),
         'projects', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM project t),
         'indexes', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY indexname), '[]'::jsonb)
                    FROM pg_indexes t WHERE schemaname = 'public'))",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap()
}

fn authenticated(user: UserId, scopes: TokenScopes) -> Caller {
    Caller {
        principal: knowell_mcp::Principal::Authenticated {
            principal: Principal::User(user),
            scopes: Some(scopes),
        },
        transport: TransportKind::StreamableHttp,
        client: Some(ClientIdentity {
            name: "synthetic-profile-reader".to_owned(),
            version: "1".to_owned(),
        }),
    }
}

async fn read_catalogue(db: &TestDb) {
    let data = tempfile::tempdir().unwrap();
    let engine = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .engine_config(&engine_config())
        .access(Arc::new(StaticAccess::local_admin(alice())))
        .build()
        .await
        .unwrap();
    let mut conn = db.store.acquire().await.unwrap();
    let organization = hierarchy::find_organization(&mut conn, &name("acme"))
        .await
        .unwrap()
        .unwrap();
    let foreign = hierarchy::create_organization(&mut conn, &name("synthetic-foreign"))
        .await
        .unwrap();
    let foreign_id = Uuid::from_u128(0xCA7A_FFFF);
    sqlx::query(
        "INSERT INTO embedding_profile (id, organization_id, name, provider, model, dimensions, input_format_version)
         VALUES ($1, $2, 'foreign-profile', 'foreign-provider', 'foreign-model', 16, 'synthetic-v1')",
    ).bind(foreign_id).bind(foreign.id).execute(&mut *conn).await.unwrap();
    let foreign_unrepresentable_id = Uuid::from_u128(0xCA7A_FFFE);
    // PostgreSQL accepts this timestamp, but Rust's OffsetDateTime does not.
    // A foreign row must be filtered in SQL before its fields are decoded.
    sqlx::query(
        "INSERT INTO embedding_profile (id, organization_id, name, provider, model, dimensions, input_format_version, created_at)
         VALUES ($1, $2, 'foreign-unrepresentable-profile', 'foreign-provider', 'foreign-unrepresentable-model',
                 16, 'synthetic-v1', '100000-01-01 00:00:00+00'::timestamptz)",
    ).bind(foreign_unrepresentable_id).bind(foreign.id).execute(&mut *conn).await.unwrap();
    drop(conn);
    let expected = seed(&db.store, organization.id).await;
    let before = snapshot(&db.store).await;
    let stats = engine.indexer().stats();
    let listed = engine
        .list_embedding_profiles(&alice_caller())
        .await
        .unwrap();
    assert_eq!(listed, expected);
    let serialized = serde_json::to_value(&listed).unwrap();
    assert_eq!(serialized[2]["created_at"], CREATED);
    for profile in serialized.as_array().unwrap() {
        let actual: std::collections::BTreeSet<&str> = profile
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            actual,
            std::collections::BTreeSet::from([
                "id",
                "name",
                "provider",
                "model",
                "dimensions",
                "input_format_version",
                "created_at",
            ])
        );
    }
    for profile in &expected {
        for selector in [
            ProfileSelector::Name(profile.name.clone()),
            ProfileSelector::Id(profile.id),
        ] {
            assert_eq!(
                engine
                    .get_embedding_profile(&alice_caller(), selector)
                    .await
                    .unwrap(),
                Some(profile.clone())
            );
        }
    }
    let missing = engine
        .get_embedding_profile(&alice_caller(), ProfileSelector::Id(Uuid::nil()))
        .await
        .unwrap();
    assert_eq!(missing, None);
    assert_eq!(
        engine
            .get_embedding_profile(&alice_caller(), ProfileSelector::Id(foreign_id))
            .await
            .unwrap(),
        missing
    );
    assert_eq!(
        engine
            .get_embedding_profile(
                &alice_caller(),
                ProfileSelector::Id(foreign_unrepresentable_id)
            )
            .await
            .unwrap(),
        missing
    );
    assert_eq!(
        engine
            .get_embedding_profile(
                &alice_caller(),
                ProfileSelector::Name(name("missing-profile"))
            )
            .await
            .unwrap(),
        missing
    );
    assert_eq!(engine.indexer().stats(), stats);
    assert_eq!(
        snapshot(&db.store).await,
        before,
        "catalogue read changed persisted metadata or indexes"
    );
    assert!(
        std::fs::read_dir(data.path()).unwrap().next().is_none(),
        "catalogue read created source index files"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalogue_returns_exact_tenant_metadata_without_source_or_provider() {
    let db = require_db!();
    read_catalogue(&db).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalogue_works_on_plain_postgres_without_vector_storage() {
    let Some(db) = TestDb::create_from(module_path!(), PLAIN_ENV).await else {
        return;
    };
    assert!(db.store.check_server().await.unwrap().vector.is_none());
    read_catalogue(&db).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalogue_uses_current_org_grants_and_read_token_scope() {
    let db = require_db!();
    let data = tempfile::tempdir().unwrap();
    let engine = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .access(Arc::new(StoreAccess::new(db.store.clone(), name("acme"))))
        .build()
        .await
        .unwrap();
    let mut conn = db.store.acquire().await.unwrap();
    let org = hierarchy::find_organization(&mut conn, &name("acme"))
        .await
        .unwrap()
        .unwrap();
    let workspace = hierarchy::create_workspace(&mut conn, org.id, &name("synthetic-workspace"))
        .await
        .unwrap();
    let source = hierarchy::create_source(
        &mut conn,
        org.id,
        SourceKind::Directory,
        "synthetic-never-open-profile-source",
    )
    .await
    .unwrap();
    let project = hierarchy::create_project(
        &mut conn,
        workspace.id,
        source.id,
        &name("synthetic-project"),
        None,
    )
    .await
    .unwrap();
    let users = [
        alice(),
        UserId::new(Uuid::from_u128(0xCA7A_2001)),
        UserId::new(Uuid::from_u128(0xCA7A_2002)),
    ];
    let scopes = [
        GrantScope::Organization,
        GrantScope::Workspace(workspace.id),
        GrantScope::Project(project.id),
    ];
    let mut grants = Vec::new();
    for (ordinal, (user, scope)) in users.iter().zip(scopes).enumerate() {
        identity::create_principal(
            &mut conn,
            org.id,
            &NewPrincipal {
                id: Some(PrincipalId(user.as_uuid())),
                kind: PrincipalKind::User,
                name: name(&format!("synthetic-reader-{ordinal}")),
                display_name: None,
            },
        )
        .await
        .unwrap();
        grants.push(
            identity::create_grant(
                &mut conn,
                &NewGrant {
                    principal: PrincipalId(user.as_uuid()),
                    role: GrantRole::Viewer,
                    scope,
                    created_by: None,
                },
            )
            .await
            .unwrap(),
        );
    }
    drop(conn);
    let expected = seed(&db.store, org.id).await;
    let before = snapshot(&db.store).await;
    let reader = authenticated(users[0], TokenScopes::read_only());
    assert_eq!(
        engine.list_embedding_profiles(&reader).await.unwrap(),
        expected
    );
    for user in users.iter().skip(1) {
        let caller = authenticated(*user, TokenScopes::read_only());
        assert!(matches!(
            engine.list_embedding_profiles(&caller).await,
            Err(ToolError::PermissionDenied(_))
        ));
        for selector in [
            ProfileSelector::Id(expected[0].id),
            ProfileSelector::Id(Uuid::nil()),
        ] {
            assert!(matches!(
                engine.get_embedding_profile(&caller, selector).await,
                Err(ToolError::PermissionDenied(_))
            ));
        }
    }
    for scope in [TokenScope::Write, TokenScope::Admin] {
        let caller = authenticated(users[0], TokenScopes::new([scope]).unwrap());
        assert!(matches!(
            engine.list_embedding_profiles(&caller).await,
            Err(ToolError::PermissionDenied(_))
        ));
        assert!(matches!(
            engine
                .get_embedding_profile(&caller, ProfileSelector::Id(expected[0].id))
                .await,
            Err(ToolError::PermissionDenied(_))
        ));
    }
    let mut conn = db.store.acquire().await.unwrap();
    assert!(
        identity::delete_grant(&mut conn, grants[0].id)
            .await
            .unwrap()
    );
    drop(conn);
    assert!(matches!(
        engine.list_embedding_profiles(&reader).await,
        Err(ToolError::PermissionDenied(_))
    ));
    assert!(matches!(
        engine
            .get_embedding_profile(&reader, ProfileSelector::Id(expected[0].id))
            .await,
        Err(ToolError::PermissionDenied(_))
    ));
    assert_eq!(snapshot(&db.store).await, before);
}

struct FixedAccess(Access);

impl AccessResolver for FixedAccess {
    fn resolve<'a>(&'a self, _: &'a Caller) -> BoxFuture<'a, Result<Access, ToolError>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalogue_denies_before_acquiring_profile_storage() {
    let db = require_db!();
    let data = tempfile::tempdir().unwrap();
    let mut engines = Vec::new();
    for scope in [
        ResourceScope::Organization,
        ResourceScope::workspace(name("synthetic-workspace")),
        ResourceScope::project(name("synthetic-workspace"), name("synthetic-project")),
    ] {
        let mut grants = GrantSet::new();
        grants.add(Grant::new(Principal::User(alice()), Role::Viewer, scope.clone()).unwrap());
        let access = StaticAccess::local_user(alice(), grants)
            .resolve(&alice_caller())
            .await
            .unwrap()
            .with_scopes(Some(if scope == ResourceScope::Organization {
                TokenScopes::new([TokenScope::Write]).unwrap()
            } else {
                TokenScopes::read_only()
            }));
        engines.push(
            Engine::builder(db.store.clone(), indexer_config(data.path()))
                .access(Arc::new(FixedAccess(access)))
                .build()
                .await
                .unwrap(),
        );
    }
    db.store.close().await;
    // Any attempted catalogue lookup now fails with Internal, rather than the
    // required PermissionDenied. The resolver itself intentionally needs no DB.
    for engine in engines {
        assert!(matches!(
            engine.list_embedding_profiles(&alice_caller()).await,
            Err(ToolError::PermissionDenied(_))
        ));
        assert!(matches!(
            engine
                .get_embedding_profile(&alice_caller(), ProfileSelector::Id(Uuid::nil()))
                .await,
            Err(ToolError::PermissionDenied(_))
        ));
    }
}

fn unsupported_timestamp_error<T>(result: Result<T, ToolError>) -> String {
    let error = match result {
        Err(error @ ToolError::Internal(_)) => error,
        Err(_) => panic!("unsupported stored timestamp returned a non-internal error"),
        Ok(_) => panic!("unsupported stored timestamp was silently returned"),
    };
    let diagnostic = error.to_string();
    let client = error.client_message("synthetic-timestamp-read");
    for text in [diagnostic.as_str(), client.as_str()] {
        for forbidden in ["100000", "infinity", "unsupported-time", "ca7a300"] {
            assert!(
                !text.contains(forbidden),
                "stored timestamp diagnostic echoed an untrusted value or row identity"
            );
        }
        assert!(!text.chars().any(char::is_control));
    }
    diagnostic
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_tenant_unsupported_timestamps_return_static_errors_without_mutation() {
    let db = require_db!();
    let data = tempfile::tempdir().unwrap();
    let engine = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .access(Arc::new(StaticAccess::local_admin(alice())))
        .build()
        .await
        .unwrap();
    let mut conn = db.store.acquire().await.unwrap();
    let organization = hierarchy::find_organization(&mut conn, &name("acme"))
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    let mut previous_error = None;
    for (ordinal, timestamp) in ["100000-01-01 00:00:00+00", "infinity"]
        .into_iter()
        .enumerate()
    {
        let id = Uuid::from_u128(0xCA7A_3000 + u128::try_from(ordinal).unwrap());
        let label = name(&format!("unsupported-time-{ordinal}"));
        let mut conn = db.store.acquire().await.unwrap();
        // Both values are legal PostgreSQL metadata. Decode failure must be
        // an ordinary error, and a catalogue read must never repair the row.
        sqlx::query(
            "INSERT INTO embedding_profile
             (id, organization_id, name, provider, model, dimensions, input_format_version, created_at)
             VALUES ($1, $2, $3, 'synthetic-provider', 'synthetic-model', 64, 'synthetic-v1', $4::text::timestamptz)",
        ).bind(id).bind(organization.id).bind(label.as_str()).bind(timestamp)
            .execute(&mut *conn).await.unwrap();
        drop(conn);
        let before = snapshot(&db.store).await;
        let stats = engine.indexer().stats();
        // A panic in any of these awaits fails the test. All three entry
        // points must instead return the same structured, value-free error.
        let listed =
            unsupported_timestamp_error(engine.list_embedding_profiles(&alice_caller()).await);
        let by_name = unsupported_timestamp_error(
            engine
                .get_embedding_profile(&alice_caller(), ProfileSelector::Name(label))
                .await,
        );
        let by_id = unsupported_timestamp_error(
            engine
                .get_embedding_profile(&alice_caller(), ProfileSelector::Id(id))
                .await,
        );
        assert_eq!(by_name, listed);
        assert_eq!(by_id, listed);
        if let Some(previous) = &previous_error {
            assert_eq!(
                &listed, previous,
                "unsupported timestamp value changed the diagnostic"
            );
        }
        previous_error = Some(listed);
        assert_eq!(engine.indexer().stats(), stats);
        assert_eq!(
            snapshot(&db.store).await,
            before,
            "invalid timestamp read rewrote metadata, jobs or indexes"
        );
        let mut conn = db.store.acquire().await.unwrap();
        sqlx::query("DELETE FROM embedding_profile WHERE id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await
            .unwrap();
    }
}
