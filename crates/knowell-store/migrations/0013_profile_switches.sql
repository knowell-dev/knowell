-- Blue-green embedding profile switches and the profile each view serves.
--
-- A switch moves views of one workspace from one profile to another. While it
-- is `building`, embeddings are built for both profiles and queries keep using
-- the serving one. Once the target covers the active generation of every
-- member view, one transaction makes it serve all of them (`active`). An
-- active switch can be rolled back until `reversible_until` by a reverse
-- switch; a building one can be cancelled. These rows are the durable truth:
-- engines resume open switches after a restart.
CREATE TYPE profile_switch_state AS ENUM ('building', 'active', 'cancelled', 'rolled_back');

CREATE TABLE profile_switch (
  id                uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id   uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  workspace_id      uuid NOT NULL,
  -- NULL when the views served no profile before.
  -- Profiles go only with their organization (they are immutable); the
  -- switches referring to them go too.
  from_profile_id   uuid REFERENCES embedding_profile (id) ON DELETE CASCADE,
  to_profile_id     uuid NOT NULL REFERENCES embedding_profile (id) ON DELETE CASCADE,
  origin            text NOT NULL CHECK (origin IN ('request', 'configuration', 'rollback')),
  rollback_of       uuid REFERENCES profile_switch (id) ON DELETE CASCADE,
  -- Who asked, as an audit label (no secrets, no free text).
  requested_by      text NOT NULL
    CHECK (requested_by ~ '^[A-Za-z0-9._:/@-]+$' AND octet_length(requested_by) <= 256),
  state             profile_switch_state NOT NULL DEFAULT 'building',
  -- How long after activation a rollback is accepted.
  retention_seconds bigint NOT NULL CHECK (retention_seconds >= 0),
  created_at        timestamptz NOT NULL DEFAULT now(),
  activated_at      timestamptz,
  reversible_until  timestamptz,
  finished_at       timestamptz,
  CONSTRAINT profile_switch_workspace_fk FOREIGN KEY (workspace_id, organization_id)
    REFERENCES workspace (id, organization_id) ON DELETE CASCADE,
  CHECK (from_profile_id IS DISTINCT FROM to_profile_id),
  CHECK ((origin = 'rollback') = (rollback_of IS NOT NULL))
);
-- At most one switch builds per workspace at a time.
CREATE UNIQUE INDEX profile_switch_one_building ON profile_switch (workspace_id)
  WHERE state = 'building';
CREATE INDEX profile_switch_organization_idx
  ON profile_switch (organization_id, created_at DESC, id DESC);

CREATE TABLE profile_switch_view (
  switch_id uuid NOT NULL REFERENCES profile_switch (id) ON DELETE CASCADE,
  view_id   uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  PRIMARY KEY (switch_id, view_id)
);
CREATE INDEX profile_switch_view_view_idx ON profile_switch_view (view_id);

-- The profile whose vectors a view's queries use (NULL: none), the profile the
-- configuration named at the last registration (a change there starts a
-- switch), and the switch that set the serving profile. Profiles are deleted
-- only with their organization, which deletes the views too.
CREATE TABLE view_embedding (
  view_id               uuid PRIMARY KEY REFERENCES view (id) ON DELETE CASCADE,
  serving_profile_id    uuid REFERENCES embedding_profile (id) ON DELETE CASCADE,
  configured_profile_id uuid REFERENCES embedding_profile (id) ON DELETE CASCADE,
  switch_id             uuid REFERENCES profile_switch (id) ON DELETE SET NULL,
  updated_at            timestamptz NOT NULL DEFAULT now()
);
