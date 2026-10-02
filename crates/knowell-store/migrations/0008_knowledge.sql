-- Knowledge (memory) records with immutable versions, evidence and history;
-- tasks and their checkpoints.
--
-- Revert (manual, destroys data):
--   DROP TABLE knowledge_history, knowledge_evidence, knowledge_record_version,
--     knowledge_record, task_checkpoint, task;
--   DROP TYPE knowledge_scope_kind, knowledge_kind, knowledge_state,
--     knowledge_action, task_status;
--   DROP FUNCTION knowell_reject_update();
--
-- The domain model lives in knowell-knowledge; the store keeps plain rows and
-- the engine maps them. Identifiers of records and tasks are chosen by the
-- caller (the domain generates them). Actors (`author`, `actor`) and the
-- structured parts of tasks are jsonb in the domain's own serialisation; the
-- store only checks their outer shape.
--
-- Scopes are stored by id, not by name, so renames keep records attached and
-- deleting a workspace, project or task deletes the records scoped to it.
-- `scope_key` is the canonical text form used for filtering:
--   org | workspace:<uuid> | project:<uuid> | task:<uuid> | user:<user key>

CREATE TYPE knowledge_scope_kind AS ENUM ('organization', 'workspace', 'project', 'task', 'user');
CREATE TYPE knowledge_kind AS ENUM ('observed', 'human', 'model_suggestion');
CREATE TYPE knowledge_state AS ENUM ('proposed', 'accepted', 'rejected', 'stale', 'superseded');
CREATE TYPE knowledge_action AS ENUM (
  'propose', 'accept', 'reject', 'mark_stale', 'revalidate', 'supersede', 'edit', 'pin'
);
CREATE TYPE task_status AS ENUM ('open', 'in_progress', 'blocked', 'done', 'abandoned');

-- Append-only tables reject UPDATE (rows still go away with their parent).
CREATE FUNCTION knowell_reject_update() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% rows are immutable; append a new row instead', TG_TABLE_NAME
    USING ERRCODE = 'restrict_violation';
END
$$;

CREATE TABLE task (
  id              uuid PRIMARY KEY,
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  workspace_id    uuid,
  -- The user the task belongs to (the same user key as user-scoped records).
  owner           text CHECK (owner <> '' AND octet_length(owner) <= 256),
  title           text NOT NULL CHECK (title <> '' AND octet_length(title) <= 1000),
  goal            text NOT NULL CHECK (goal <> '' AND octet_length(goal) <= 65536),
  status          task_status NOT NULL,
  notes           jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(notes) = 'array'),
  decisions       uuid[] NOT NULL DEFAULT '{}',
  open_questions  jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(open_questions) = 'array'),
  related_symbols text[] NOT NULL DEFAULT '{}',
  related_files   jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(related_files) = 'array'),
  view_manifest   jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(view_manifest) = 'array'),
  -- Optimistic-concurrency token: bumped by every update.
  revision        bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
  created_at      timestamptz NOT NULL,
  updated_at      timestamptz NOT NULL,
  UNIQUE (id, organization_id),
  CONSTRAINT task_workspace_fk FOREIGN KEY (workspace_id, organization_id)
    REFERENCES workspace (id, organization_id) ON DELETE CASCADE
);
CREATE INDEX task_updated_idx ON task (organization_id, updated_at DESC, id DESC);
CREATE INDEX task_owner_idx ON task (organization_id, owner) WHERE owner IS NOT NULL;
CREATE INDEX task_workspace_idx ON task (workspace_id) WHERE workspace_id IS NOT NULL;

-- Saved progress of a task, numbered 1, 2, ... per task. Never changed.
CREATE TABLE task_checkpoint (
  task_id    uuid NOT NULL REFERENCES task (id) ON DELETE CASCADE,
  seq        bigint NOT NULL CHECK (seq >= 1),
  at         timestamptz NOT NULL,
  summary    text NOT NULL CHECK (summary <> '' AND octet_length(summary) <= 65536),
  decisions  uuid[] NOT NULL DEFAULT '{}',
  next_steps text[] NOT NULL DEFAULT '{}',
  -- The view manifest pins (project, view, commit, local generation).
  manifest   jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(manifest) = 'array'),
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (task_id, seq)
);
CREATE TRIGGER task_checkpoint_append_only
  BEFORE UPDATE ON task_checkpoint
  FOR EACH ROW EXECUTE FUNCTION knowell_reject_update();

-- The current state of a record. Its content (title, body, tags) is also
-- kept per version in knowledge_record_version; the row repeats the current
-- version's content so it can be searched and listed without a join.
CREATE TABLE knowledge_record (
  id              uuid PRIMARY KEY,
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  scope_kind      knowledge_scope_kind NOT NULL,
  -- Set for workspace and project scopes (for a project: its workspace).
  workspace_id    uuid,
  project_id      uuid REFERENCES project (id) ON DELETE CASCADE,
  task_id         uuid,
  user_key        text CHECK (user_key <> '' AND octet_length(user_key) <= 256),
  scope_key       text NOT NULL GENERATED ALWAYS AS (
                    CASE scope_kind
                      WHEN 'organization' THEN 'org'
                      WHEN 'workspace' THEN 'workspace:' || workspace_id::text
                      WHEN 'project' THEN 'project:' || project_id::text
                      WHEN 'task' THEN 'task:' || task_id::text
                      ELSE 'user:' || user_key
                    END) STORED,
  kind            knowledge_kind NOT NULL,
  subject         text NOT NULL
                    CHECK (subject ~ '^[a-z0-9_-]+(\.[a-z0-9_-]+)*$' AND octet_length(subject) <= 128),
  title           text NOT NULL CHECK (title <> '' AND octet_length(title) <= 1000),
  body            text NOT NULL CHECK (octet_length(body) <= 262144),
  state           knowledge_state NOT NULL,
  -- Content version of the domain model (1, 2, ...): bumped by edits.
  version         integer NOT NULL CHECK (version >= 1),
  -- Optimistic-concurrency token: bumped by every update, including state
  -- changes that keep the content version.
  revision        bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
  author          jsonb NOT NULL,
  pinned          boolean NOT NULL DEFAULT false,
  tags            text[] NOT NULL DEFAULT '{}',
  related_symbols text[] NOT NULL DEFAULT '{}',
  superseded_by   uuid,
  created_at      timestamptz NOT NULL,
  updated_at      timestamptz NOT NULL,
  -- Full-text search for read_memory (`simple` configuration: no stemming,
  -- no stop words). Subjects are split at dots so `payments.idempotency`
  -- matches `idempotency`.
  search          tsvector NOT NULL GENERATED ALWAYS AS (
                    setweight(to_tsvector('simple'::regconfig, title), 'A')
                    || setweight(to_tsvector('simple'::regconfig, replace(subject, '.', ' ')), 'A')
                    || setweight(to_tsvector('simple'::regconfig, body), 'B')) STORED,
  UNIQUE (id, organization_id),
  CONSTRAINT knowledge_record_workspace_fk FOREIGN KEY (workspace_id, organization_id)
    REFERENCES workspace (id, organization_id) ON DELETE CASCADE,
  CONSTRAINT knowledge_record_task_fk FOREIGN KEY (task_id, organization_id)
    REFERENCES task (id, organization_id) ON DELETE CASCADE,
  CONSTRAINT knowledge_record_superseded_by_fk FOREIGN KEY (superseded_by, organization_id)
    REFERENCES knowledge_record (id, organization_id) ON DELETE SET NULL (superseded_by),
  CONSTRAINT knowledge_record_not_self_superseded CHECK (superseded_by IS DISTINCT FROM id),
  CONSTRAINT knowledge_record_scope_keys CHECK (
    CASE scope_kind
      WHEN 'organization' THEN workspace_id IS NULL AND project_id IS NULL
                               AND task_id IS NULL AND user_key IS NULL
      WHEN 'workspace' THEN workspace_id IS NOT NULL AND project_id IS NULL
                            AND task_id IS NULL AND user_key IS NULL
      WHEN 'project' THEN workspace_id IS NOT NULL AND project_id IS NOT NULL
                          AND task_id IS NULL AND user_key IS NULL
      WHEN 'task' THEN workspace_id IS NULL AND project_id IS NULL
                       AND task_id IS NOT NULL AND user_key IS NULL
      ELSE workspace_id IS NULL AND project_id IS NULL
           AND task_id IS NULL AND user_key IS NOT NULL
    END)
);
CREATE INDEX knowledge_record_scope_idx ON knowledge_record (organization_id, scope_key, state);
CREATE INDEX knowledge_record_subject_idx ON knowledge_record (organization_id, subject);
CREATE INDEX knowledge_record_updated_idx ON knowledge_record (organization_id, updated_at DESC, id DESC);
CREATE INDEX knowledge_record_search_idx ON knowledge_record USING gin (search);
-- Staleness: which records are about a symbol that changed.
CREATE INDEX knowledge_record_symbols_idx ON knowledge_record USING gin (related_symbols);
CREATE INDEX knowledge_record_workspace_idx ON knowledge_record (workspace_id) WHERE workspace_id IS NOT NULL;
CREATE INDEX knowledge_record_project_idx ON knowledge_record (project_id) WHERE project_id IS NOT NULL;
CREATE INDEX knowledge_record_task_idx ON knowledge_record (task_id) WHERE task_id IS NOT NULL;
CREATE INDEX knowledge_record_superseded_idx ON knowledge_record (superseded_by) WHERE superseded_by IS NOT NULL;

-- Every content version of a record, including the current one. Never
-- changed: an edit appends version n + 1.
CREATE TABLE knowledge_record_version (
  record_id  uuid NOT NULL REFERENCES knowledge_record (id) ON DELETE CASCADE,
  version    integer NOT NULL CHECK (version >= 1),
  title      text NOT NULL CHECK (title <> '' AND octet_length(title) <= 1000),
  body       text NOT NULL CHECK (octet_length(body) <= 262144),
  tags       text[] NOT NULL DEFAULT '{}',
  -- When this version became current; the next version's created_at is
  -- when it was replaced.
  created_at timestamptz NOT NULL,
  PRIMARY KEY (record_id, version)
);
CREATE TRIGGER knowledge_record_version_append_only
  BEFORE UPDATE ON knowledge_record_version
  FOR EACH ROW EXECUTE FUNCTION knowell_reject_update();

-- The code a record version is based on. A line range alone never identifies
-- code: it is pinned to a view, a commit and the hash of the file content.
CREATE TABLE knowledge_evidence (
  record_id    uuid NOT NULL,
  version      integer NOT NULL,
  ordinal      integer NOT NULL CHECK (ordinal >= 0),
  project_id   uuid NOT NULL REFERENCES project (id) ON DELETE CASCADE,
  -- The view the evidence was read from, as the engine names it.
  view_key     text NOT NULL CHECK (view_key <> '' AND octet_length(view_key) <= 256),
  commit_id    text NOT NULL CHECK (commit_id ~ '^[0-9a-f]{7,64}$'),
  path         text NOT NULL CHECK (path <> '' AND path !~ '^/'),
  start_line   integer NOT NULL CHECK (start_line >= 1),
  end_line     integer NOT NULL,
  content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  PRIMARY KEY (record_id, version, ordinal),
  FOREIGN KEY (record_id, version) REFERENCES knowledge_record_version (record_id, version) ON DELETE CASCADE,
  CHECK (end_line >= start_line)
);
-- Staleness: which records cite a file whose content just changed.
CREATE INDEX knowledge_evidence_file_idx ON knowledge_evidence (project_id, path, content_hash);
CREATE TRIGGER knowledge_evidence_append_only
  BEFORE UPDATE ON knowledge_evidence
  FOR EACH ROW EXECUTE FUNCTION knowell_reject_update();

-- Every state transition of a record, oldest first (by id). Never changed.
CREATE TABLE knowledge_history (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  record_id  uuid NOT NULL REFERENCES knowledge_record (id) ON DELETE CASCADE,
  at         timestamptz NOT NULL,
  actor      jsonb NOT NULL,
  action     knowledge_action NOT NULL,
  -- NULL for the creation entry.
  from_state knowledge_state,
  to_state   knowledge_state NOT NULL,
  -- Content version after the action.
  version    integer NOT NULL CHECK (version >= 1),
  reason     text NOT NULL CHECK (reason <> '' AND octet_length(reason) <= 4096)
);
CREATE INDEX knowledge_history_record_idx ON knowledge_history (record_id, id);
CREATE TRIGGER knowledge_history_append_only
  BEFORE UPDATE ON knowledge_history
  FOR EACH ROW EXECUTE FUNCTION knowell_reject_update();
