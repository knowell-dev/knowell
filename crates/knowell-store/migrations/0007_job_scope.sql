-- Tenant attribution of jobs, and indexes for job listings.
--
-- Revert (manual):
--   DROP INDEX job_scope_idx, job_created_idx, job_queued_created_idx;
--   ALTER TABLE job DROP COLUMN workspace_id, DROP COLUMN organization_id;
--
-- Jobs enqueued before this migration, or without a scope, keep both columns
-- NULL ("unscoped"): listings decide explicitly whether to include them.
-- A workspace-scoped job always names its organization too, and the
-- composite foreign key keeps both in the same tenant. Deleting the
-- organization or workspace deletes its jobs (their payloads point at data
-- that no longer exists).

ALTER TABLE job
  ADD COLUMN organization_id uuid REFERENCES organization (id) ON DELETE CASCADE,
  ADD COLUMN workspace_id    uuid,
  ADD CONSTRAINT job_workspace_fk FOREIGN KEY (workspace_id, organization_id)
    REFERENCES workspace (id, organization_id) ON DELETE CASCADE,
  ADD CONSTRAINT job_workspace_needs_organization
    CHECK (workspace_id IS NULL OR organization_id IS NOT NULL);

-- Listings go newest first with (created_at, id) keyset pagination.
CREATE INDEX job_created_idx ON job (created_at DESC, id DESC);
CREATE INDEX job_scope_idx ON job (organization_id, workspace_id, created_at DESC, id DESC)
  WHERE organization_id IS NOT NULL;
-- Age of the oldest waiting job (queue health).
CREATE INDEX job_queued_created_idx ON job (created_at) WHERE state = 'queued';
