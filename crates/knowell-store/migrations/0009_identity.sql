-- Identity and access: principals, grants, API tokens and the audit log.
--
-- Revert (manual, destroys data):
--   DROP TABLE audit_log, api_token, access_grant, principal;
--   DROP TYPE principal_kind, grant_role, grant_level, token_scope;
--   DROP FUNCTION knowell_audit_log_guard();
--
-- The domain model lives in knowell-auth; the server maps these rows to it.
-- Agent sessions are not principals: an agent acts for a user, so an agent
-- token belongs to that user's principal and names the agent client and
-- session on the token itself.

CREATE TYPE principal_kind AS ENUM ('user', 'service_account');
CREATE TYPE grant_role AS ENUM ('viewer', 'member', 'maintainer', 'admin');
CREATE TYPE grant_level AS ENUM ('organization', 'workspace', 'project');
CREATE TYPE token_scope AS ENUM ('read', 'write', 'admin');

-- A user or service account of one organization. The id is the
-- knowell-auth UserId / ServiceAccountId.
CREATE TABLE principal (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  kind            principal_kind NOT NULL,
  name            text NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  display_name    text CHECK (display_name <> '' AND octet_length(display_name) <= 200),
  -- A disabled principal keeps its records but cannot authenticate (its
  -- tokens read as revoked) and holds no grants.
  disabled_at     timestamptz,
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (organization_id, name),
  UNIQUE (id, organization_id)
);

-- A role given to a principal at a scope. ("grant" is an SQL keyword.)
-- Scopes are stored by id; listings join the current names.
CREATE TABLE access_grant (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id uuid NOT NULL,
  principal_id    uuid NOT NULL,
  role            grant_role NOT NULL,
  level           grant_level NOT NULL,
  -- Set for workspace and project grants (for a project: its workspace).
  workspace_id    uuid,
  project_id      uuid REFERENCES project (id) ON DELETE CASCADE,
  created_by      uuid REFERENCES principal (id) ON DELETE SET NULL,
  created_at      timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT access_grant_principal_fk FOREIGN KEY (principal_id, organization_id)
    REFERENCES principal (id, organization_id) ON DELETE CASCADE,
  CONSTRAINT access_grant_workspace_fk FOREIGN KEY (workspace_id, organization_id)
    REFERENCES workspace (id, organization_id) ON DELETE CASCADE,
  CONSTRAINT access_grant_scope_keys CHECK (
    (level = 'organization' AND workspace_id IS NULL AND project_id IS NULL)
    OR (level = 'workspace' AND workspace_id IS NOT NULL AND project_id IS NULL)
    OR (level = 'project' AND workspace_id IS NOT NULL AND project_id IS NOT NULL)),
  CONSTRAINT access_grant_unique UNIQUE NULLS NOT DISTINCT (principal_id, role, level, workspace_id, project_id)
);
CREATE INDEX access_grant_principal_idx ON access_grant (principal_id);
CREATE INDEX access_grant_workspace_idx ON access_grant (workspace_id) WHERE workspace_id IS NOT NULL;
CREATE INDEX access_grant_project_idx ON access_grant (project_id) WHERE project_id IS NOT NULL;

-- API tokens. Never the plaintext: the lookup prefix (`kn_` + 8 characters,
-- not unique) and the keyed hash made with the server's pepper. The id is the
-- knowell-auth TokenId.
CREATE TABLE api_token (
  id              uuid PRIMARY KEY,
  organization_id uuid NOT NULL,
  principal_id    uuid NOT NULL,
  -- Both set for an agent token (acting for the user principal_id).
  agent_client    text CHECK (agent_client ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  agent_session   uuid,
  prefix          text NOT NULL CHECK (prefix ~ '^kn_[a-z2-7]{8}$'),
  key_hash        bytea NOT NULL UNIQUE CHECK (octet_length(key_hash) = 32),
  scopes          token_scope[] NOT NULL
                    CHECK (cardinality(scopes) >= 1 AND array_position(scopes, NULL) IS NULL),
  label           text CHECK (label <> '' AND octet_length(label) <= 200),
  created_by      uuid REFERENCES principal (id) ON DELETE SET NULL,
  created_at      timestamptz NOT NULL,
  expires_at      timestamptz CHECK (expires_at > created_at),
  revoked_at      timestamptz,
  last_used_at    timestamptz,
  CONSTRAINT api_token_principal_fk FOREIGN KEY (principal_id, organization_id)
    REFERENCES principal (id, organization_id) ON DELETE CASCADE,
  CONSTRAINT api_token_agent_pair CHECK ((agent_client IS NULL) = (agent_session IS NULL)),
  -- The knowell-auth rules for agent tokens, enforced again here.
  CONSTRAINT api_token_agent_limits CHECK (
    agent_client IS NULL
    OR (expires_at IS NOT NULL
        AND expires_at <= created_at + interval '24 hours'
        AND NOT ('admin' = ANY (scopes))))
);
CREATE INDEX api_token_prefix_idx ON api_token (prefix);
CREATE INDEX api_token_principal_idx ON api_token (principal_id);

-- Authorization outcomes, append-only. Only identifiers, codes and the
-- request id: every text column is restricted to a small alphabet, so free
-- text (and with it secrets) cannot be written. organization_id has no
-- foreign key on purpose: the log outlives what it describes. Rows are
-- removed only by the retention function in knowell-store, which sets
-- `knowell.audit_prune` for its own transaction.
CREATE TABLE audit_log (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  -- NULL for events recorded before the organization existed.
  organization_id uuid,
  at              timestamptz NOT NULL,
  -- knowell-auth text form: user:<uuid>, service_account:<uuid>,
  -- agent:<user>:<client>:<session>.
  actor           text NOT NULL CHECK (actor ~ '^[A-Za-z0-9._:/@-]+$' AND octet_length(actor) <= 512),
  -- The acting principal (an agent's user), for per-principal queries.
  principal_id    uuid,
  action          text NOT NULL CHECK (action ~ '^[A-Za-z0-9._:/@-]+$' AND octet_length(action) <= 256),
  resource        text NOT NULL CHECK (resource ~ '^[A-Za-z0-9._:/@-]+$' AND octet_length(resource) <= 512),
  allowed         boolean NOT NULL,
  reason          text NOT NULL CHECK (reason ~ '^[a-z0-9_]{1,64}$'),
  request_id      text NOT NULL CHECK (request_id ~ '^[A-Za-z0-9._:-]+$' AND octet_length(request_id) <= 128),
  recorded_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_org_idx ON audit_log (organization_id, at DESC, id DESC);
CREATE INDEX audit_log_principal_idx ON audit_log (principal_id, at DESC, id DESC)
  WHERE principal_id IS NOT NULL;
CREATE INDEX audit_log_at_idx ON audit_log (at);

CREATE FUNCTION knowell_audit_log_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND current_setting('knowell.audit_prune', true) = 'on' THEN
    RETURN OLD;
  END IF;
  RAISE EXCEPTION 'the audit log is append-only'
    USING ERRCODE = 'restrict_violation';
END
$$;

CREATE TRIGGER audit_log_append_only
  BEFORE UPDATE OR DELETE ON audit_log
  FOR EACH ROW EXECUTE FUNCTION knowell_audit_log_guard();
CREATE TRIGGER audit_log_no_truncate
  BEFORE TRUNCATE ON audit_log
  FOR EACH STATEMENT EXECUTE FUNCTION knowell_audit_log_guard();
