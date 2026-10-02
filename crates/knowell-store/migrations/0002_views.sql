-- Views, generations (with the activation fence) and view manifests.
--
-- Revert (manual, destroys data):
--   DROP TABLE view_manifest_entry, view_manifest, view_generation, view;

-- A view is what one project looks like when following one track target
-- (`branch:development`, `tag:v2.1.0`, `worktree`, ...). Its content is
-- versioned by generations: each indexing run of the view gets the next
-- generation number, and exactly one generation is active at a time.
CREATE TABLE view (
  id                 uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  project_id         uuid NOT NULL REFERENCES project (id) ON DELETE CASCADE,
  -- Canonical text of knowell_core::TrackTarget.
  track_target       text NOT NULL CHECK (track_target <> ''),
  kind               view_kind NOT NULL,
  -- Generation counter: the highest generation ever allocated (0 = none).
  last_generation    bigint NOT NULL DEFAULT 0 CHECK (last_generation >= 0),
  -- The generation queries use by default, and the commit it was built from.
  active_generation  bigint CHECK (active_generation > 0),
  active_commit      text CHECK (active_commit ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  -- The newest commit seen on the tracked ref (may be ahead of the active one).
  latest_seen_commit text CHECK (latest_seen_commit ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  created_at         timestamptz NOT NULL DEFAULT now(),
  updated_at         timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, track_target),
  CHECK (active_generation IS NULL OR active_generation <= last_generation)
);

CREATE TABLE view_generation (
  view_id         uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  generation      bigint NOT NULL CHECK (generation > 0),
  -- NULL for directory sources, which have no commits.
  resolved_commit text CHECK (resolved_commit ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  state           generation_state NOT NULL DEFAULT 'building',
  error           text,
  created_at      timestamptz NOT NULL DEFAULT now(),
  activated_at    timestamptz,
  finished_at     timestamptz,
  PRIMARY KEY (view_id, generation)
);
-- Generation-scoped rows (files, occurrences, edges, contracts) are stored as
-- validity intervals, which is only sound when one generation per view is
-- being written at a time. The database enforces it.
CREATE UNIQUE INDEX view_generation_one_building ON view_generation (view_id) WHERE state = 'building';
CREATE UNIQUE INDEX view_generation_one_active ON view_generation (view_id) WHERE state = 'active';

-- A manifest pins, for each project, one view generation and its commit, so a
-- multi-project query (or a named release view) reads a consistent snapshot.
CREATE TABLE view_manifest (
  id           uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  workspace_id uuid NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
  name         text CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX view_manifest_name ON view_manifest (workspace_id, name) WHERE name IS NOT NULL;

CREATE TABLE view_manifest_entry (
  manifest_id     uuid NOT NULL REFERENCES view_manifest (id) ON DELETE CASCADE,
  project_id      uuid NOT NULL REFERENCES project (id) ON DELETE CASCADE,
  view_id         uuid NOT NULL,
  generation      bigint NOT NULL,
  resolved_commit text,
  PRIMARY KEY (manifest_id, project_id),
  -- A pinned generation cannot be pruned while the manifest exists.
  FOREIGN KEY (view_id, generation) REFERENCES view_generation (view_id, generation)
);
CREATE INDEX view_manifest_entry_view_idx ON view_manifest_entry (view_id, generation);
