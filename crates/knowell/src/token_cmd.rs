//! Local administrator operations; no credential values go to stdout or logs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use knowell_auth::{Pepper, Principal, ServiceAccountId, TokenScope, TokenScopes, UserId};
use knowell_config::EngineConfig;
use knowell_core::Name;
use knowell_store::identity::{self, GrantScope, NewApiToken, NewGrant, NewPrincipal};
use knowell_store::{ApiTokenId, ApiTokenScope, GrantRole, PrincipalKind, Store, hierarchy};
use secrecy::ExposeSecret;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{db, env::Env, output::Output};

#[derive(Debug, Subcommand)]
pub(crate) enum TokenCommand {
    /// Issue a token into a new private file; never prints its value.
    Create(CreateArgs),
    /// List metadata only (no hashes or credential values).
    List(SelectArgs),
    /// Revoke one token in this organization, immediately and idempotently.
    Revoke(RevokeArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SelectArgs {
    #[arg(long, default_value = "local")]
    organization: Name,
    /// Restrict the list to a stored principal's login name.
    #[arg(long)]
    principal: Option<Name>,
}

#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    #[arg(long, default_value = "local")]
    organization: Name,
    /// Stored principal's login name, or new user name with --create-user.
    #[arg(long)]
    principal: Name,
    /// Create a user and its initial grant. Requires an explicit --role.
    #[arg(long, requires = "role")]
    create_user: bool,
    #[arg(long, value_enum, requires = "create_user")]
    role: Option<RoleArg>,
    /// Limit the new user's grant to this workspace.
    #[arg(long = "grant-workspace", requires = "create_user")]
    workspace: Option<Name>,
    /// Limit the new user's grant to this project within --grant-workspace.
    #[arg(long = "grant-project", requires_all = ["workspace", "create_user"])]
    project: Option<Name>,
    #[arg(long, value_enum, value_delimiter = ',', default_value = "read")]
    scopes: Vec<ScopeArg>,
    /// Lifetime in hours (1-8760); defaults to 30 days.
    #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(1..=8760))]
    expires_hours: u32,
    /// New file in an existing directory; existing files are never replaced.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct RevokeArgs {
    #[arg(long, default_value = "local")]
    organization: Name,
    id: Uuid,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    Read,
    Write,
    Admin,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RoleArg {
    Viewer,
    Member,
    Maintainer,
    Admin,
}

pub(crate) fn pepper(config: &EngineConfig) -> anyhow::Result<Pepper> {
    let reference = config.server.token_pepper.as_ref().context(
        "configure server.token_pepper as an env:NAME or file:/path reference before using tokens",
    )?;
    let value = knowell_secrets::resolve(reference).context("cannot resolve the token pepper")?;
    Pepper::new(value.expose_secret().as_bytes()).map_err(anyhow::Error::new)
}

pub(crate) fn run(cmd: TokenCommand, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let config = env.require_engine()?;
    db::runtime()?.block_on(async {
        let store = db::connect(env, &config, std::time::Duration::from_secs(15), 2).await?;
        store.validate_schema().await?;
        let result = match cmd {
            TokenCommand::Create(args) => create(&store, &config, args, out).await,
            TokenCommand::List(args) => list(&store, args, out).await,
            TokenCommand::Revoke(args) => revoke(&store, args, out).await,
        };
        store.close().await;
        result
    })?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

async fn create(
    store: &Store,
    config: &EngineConfig,
    args: CreateArgs,
    out: &mut Output,
) -> anyhow::Result<()> {
    let pepper = pepper(config)?;
    let mut tx = store.begin().await?;
    let org = match hierarchy::find_organization(&mut tx, &args.organization).await? {
        Some(org) => org,
        None if args.create_user => {
            hierarchy::create_organization(&mut tx, &args.organization).await?
        }
        None => bail!("the organization does not exist"),
    };
    let existing = identity::find_principal(&mut tx, org.id, &args.principal).await?;
    let user = match (existing, args.create_user) {
        (Some(_), true) => bail!(
            "the principal already exists; omit --create-user to issue a token without changing its grants"
        ),
        (Some(user), false) => user,
        (None, false) => {
            bail!("the principal does not exist; use --create-user with an explicit --role")
        }
        (None, true) => {
            let role = match args.role.context("--create-user requires --role")? {
                RoleArg::Viewer => GrantRole::Viewer,
                RoleArg::Member => GrantRole::Member,
                RoleArg::Maintainer => GrantRole::Maintainer,
                RoleArg::Admin => GrantRole::Admin,
            };
            let scope = match &args.workspace {
                None => GrantScope::Organization,
                Some(workspace) => {
                    let workspace = hierarchy::find_workspace(&mut tx, org.id, workspace)
                        .await?
                        .context("the workspace does not exist")?;
                    match &args.project {
                        None => GrantScope::Workspace(workspace.id),
                        Some(project) => GrantScope::Project(
                            hierarchy::find_project(&mut tx, workspace.id, project)
                                .await?
                                .context("the project does not exist")?
                                .id,
                        ),
                    }
                }
            };
            let user = identity::create_principal(
                &mut tx,
                org.id,
                &NewPrincipal {
                    id: None,
                    kind: PrincipalKind::User,
                    name: args.principal,
                    display_name: None,
                },
            )
            .await?;
            identity::create_grant(
                &mut tx,
                &NewGrant {
                    principal: user.id,
                    role,
                    scope,
                    created_by: None,
                },
            )
            .await?;
            user
        }
    };
    if user.disabled_at.is_some() {
        bail!("cannot issue a token for a disabled principal");
    }
    let principal = match user.kind {
        PrincipalKind::User => Principal::User(UserId::new(user.id.as_uuid())),
        PrincipalKind::ServiceAccount => {
            Principal::ServiceAccount(ServiceAccountId::new(user.id.as_uuid()))
        }
    };
    let scopes = TokenScopes::new(args.scopes.iter().map(|s| match s {
        ScopeArg::Read => TokenScope::Read,
        ScopeArg::Write => TokenScope::Write,
        ScopeArg::Admin => TokenScope::Admin,
    }))
    .context("at least one token scope is required")?;
    let now = OffsetDateTime::now_utc();
    let expires = now
        .checked_add(time::Duration::hours(i64::from(args.expires_hours)))
        .context("token expiry is out of range")?;
    let (plain, token) =
        knowell_auth::issue_token(&principal, scopes, Some(expires), now, &pepper)?;
    identity::insert_api_token(
        &mut tx,
        &NewApiToken {
            id: ApiTokenId(token.id.as_uuid()),
            principal: user.id,
            agent: None,
            prefix: token.prefix.clone(),
            key_hash: *token.hash.as_bytes(),
            scopes: token
                .scopes
                .iter()
                .map(|s| match s {
                    TokenScope::Read => ApiTokenScope::Read,
                    TokenScope::Write => ApiTokenScope::Write,
                    TokenScope::Admin => ApiTokenScope::Admin,
                })
                .collect(),
            label: None,
            created_by: None,
            created_at: now,
            expires_at: Some(expires),
        },
    )
    .await?;
    record_change(&mut tx, org.id, token.id.as_uuid(), "token_create").await?;
    // A failed output write rolls back both token and optional bootstrap user.
    // A failed commit can leave only an unusable credential, never a live token
    // whose value the administrator could not receive.
    write_private_new(&args.output, plain.expose())?;
    tx.commit().await.context(
        "cannot commit token creation; the output file may contain an unusable credential",
    )?;
    out.line(format!(
        "created token {}; credential saved to {}",
        token.id,
        args.output.display()
    ))?;
    Ok(())
}

async fn list(store: &Store, args: SelectArgs, out: &mut Output) -> anyhow::Result<()> {
    let mut conn = store.acquire().await?;
    let org = hierarchy::find_organization(&mut conn, &args.organization)
        .await?
        .context("the organization does not exist")?;
    let principal = match args.principal {
        Some(name) => Some(
            identity::find_principal(&mut conn, org.id, &name)
                .await?
                .context("the principal does not exist")?
                .id,
        ),
        None => None,
    };
    for token in identity::list_api_tokens(&mut conn, org.id, principal).await? {
        let state = if token.effective_revoked_at().is_some() {
            "revoked"
        } else if token
            .expires_at
            .is_some_and(|t| t <= OffsetDateTime::now_utc())
        {
            "expired"
        } else {
            "active"
        };
        out.line(format!(
            "{} principal={} state={} scopes={:?}",
            token.id, token.principal, state, token.scopes
        ))?;
    }
    Ok(())
}

async fn revoke(store: &Store, args: RevokeArgs, out: &mut Output) -> anyhow::Result<()> {
    let mut tx = store.begin().await?;
    let org = hierarchy::find_organization(&mut tx, &args.organization)
        .await?
        .context("the organization does not exist")?;
    let id = ApiTokenId(args.id);
    let token = identity::get_api_token(&mut tx, id)
        .await?
        .filter(|t| t.organization == org.id)
        .context("the token does not exist in this organization")?;
    identity::revoke_api_token(&mut tx, token.id, OffsetDateTime::now_utc()).await?;
    record_change(&mut tx, org.id, args.id, "token_revoke").await?;
    tx.commit().await?;
    out.line(format!("revoked token {id}"))?;
    Ok(())
}

async fn record_change(
    conn: &mut knowell_store::PgConnection,
    organization: knowell_store::OrganizationId,
    token: Uuid,
    action: &str,
) -> anyhow::Result<()> {
    knowell_store::audit::append_audit(
        conn,
        &[knowell_store::audit::NewAuditEntry {
            organization: Some(organization),
            at: OffsetDateTime::now_utc(),
            actor: "local-admin".into(),
            principal: None,
            action: action.into(),
            resource: format!("token:{token}"),
            allowed: true,
            reason: "local_database_admin".into(),
            request_id: Uuid::now_v7().to_string(),
        }],
    )
    .await?;
    Ok(())
}

fn write_private_new(path: &Path, value: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file =
        tempfile::NamedTempFile::new_in(parent).context("cannot create the credential file")?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let user = std::env::var("USERNAME").context("cannot identify the file owner")?;
        let status = std::process::Command::new("icacls")
            .arg(file.path())
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:F"))
            .creation_flags(0x0800_0000)
            .output()
            .context("cannot restrict the credential file")?;
        if !status.status.success() {
            bail!("cannot restrict the credential file to its owner");
        }
    }
    // tempfile creates Unix files with mode 0600. On Windows the empty
    // file's ACL is restricted above before any credential bytes are written.
    file.write_all(value.as_bytes())
        .context("cannot write the credential file")?;
    file.as_file()
        .sync_all()
        .context("cannot sync the credential file")?;
    file.persist_noclobber(path)
        .map_err(|e| e.error)
        .context("cannot save the credential file; choose a new path in an existing directory")?;
    Ok(())
}
