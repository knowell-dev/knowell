-- Foundation: extensions, identifier generator, shared enums and the
-- organization -> workspace / source -> project hierarchy.
--
-- Migrations are append-only: never edit a file that has been released; add a
-- new numbered file instead. Each file documents how to revert it by hand.
--
-- Revert (manual, destroys data):
--   DROP TABLE project, source, workspace, organization;
--   DROP TYPE source_kind, view_kind, generation_state, occurrence_role,
--     node_kind, evidence_type, edge_resolution, contract_kind, contract_role,
--     job_state;
--   DROP FUNCTION knowell_uuidv7();
--   (the `vector` extension may be shared with other schemas; drop it only
--   if nothing else uses it)

-- Vector support is installed separately when pgvector is available.

-- UUIDv7 (RFC 9562): 48-bit Unix millisecond timestamp, version 7, random
-- rest. PostgreSQL 18 has a built-in uuidv7(), 17 does not; this function
-- works on both so ids sort by creation time on every supported server.
CREATE FUNCTION knowell_uuidv7() RETURNS uuid
LANGUAGE sql VOLATILE PARALLEL SAFE AS $$
  SELECT encode(
    set_bit(
      set_bit(
        overlay(uuid_send(gen_random_uuid())
                PLACING substring(int8send(floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint) FROM 3)
                FROM 1 FOR 6),
        52, 1),
      53, 1),
    'hex')::uuid
$$;

CREATE TYPE source_kind AS ENUM ('git', 'directory');
CREATE TYPE view_kind AS ENUM ('branch', 'remote', 'tag', 'commit', 'worktree');
CREATE TYPE generation_state AS ENUM ('building', 'active', 'retired', 'failed');
CREATE TYPE occurrence_role AS ENUM ('definition', 'reference');
CREATE TYPE node_kind AS ENUM ('symbol', 'file', 'project', 'contract', 'name');
CREATE TYPE evidence_type AS ENUM (
  'semantic_resolved',
  'contract_derived',
  'syntactic',
  'heuristic',
  'model_suggestion',
  'runtime_observed'
);
CREATE TYPE edge_resolution AS ENUM ('resolved', 'ambiguous', 'unresolved');
CREATE TYPE contract_kind AS ENUM ('endpoint', 'topic', 'rpc', 'table', 'env_name', 'i18n_key', 'package');
CREATE TYPE contract_role AS ENUM ('producer', 'consumer');
CREATE TYPE job_state AS ENUM ('queued', 'running', 'succeeded', 'failed', 'dead', 'cancelled');

-- Tenant and security boundary. Nothing (content, vectors, caches) is shared
-- across organizations.
CREATE TABLE organization (
  id         uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  name       text NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workspace (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  name            text NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (organization_id, name),
  -- Target of the composite foreign key that keeps projects inside one tenant.
  UNIQUE (id, organization_id)
);

-- A git repository or plain directory. One source can back projects in
-- several workspaces of the same organization.
CREATE TABLE source (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  kind            source_kind NOT NULL,
  location        text NOT NULL CHECK (location <> ''),
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (organization_id, location),
  UNIQUE (id, organization_id)
);

-- A project is a root inside a source (the whole repository or a monorepo
-- package). `root_path` is '' for the source root, otherwise a normalised
-- '/'-separated relative path.
CREATE TABLE project (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id uuid NOT NULL,
  workspace_id    uuid NOT NULL,
  source_id       uuid NOT NULL,
  name            text NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  root_path       text NOT NULL DEFAULT '' CHECK (root_path !~ '^/'),
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (workspace_id, name),
  FOREIGN KEY (workspace_id, organization_id) REFERENCES workspace (id, organization_id) ON DELETE CASCADE,
  -- NO ACTION (not RESTRICT): a source in use cannot be deleted on its own,
  -- but deleting the organization cascades through both paths.
  FOREIGN KEY (source_id, organization_id) REFERENCES source (id, organization_id)
);
CREATE INDEX project_source_idx ON project (source_id);
