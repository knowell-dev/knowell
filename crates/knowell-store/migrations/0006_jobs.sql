-- Durable job queue.
--
-- Revert (manual, destroys data):
--   DROP TABLE job;
--
-- States: queued -> running -> succeeded | failed (retry pending, claimable
-- again after run_after) | dead (attempts exhausted) | cancelled.
-- Workers claim with FOR UPDATE SKIP LOCKED and hold a lease that they
-- extend with heartbeats; expired leases are reclaimed after a crash.

CREATE TABLE job (
  id               uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  kind             text NOT NULL CHECK (kind <> ''),
  payload          jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Larger runs first.
  priority         integer NOT NULL DEFAULT 0,
  state            job_state NOT NULL DEFAULT 'queued',
  attempts         integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  max_attempts     integer NOT NULL CHECK (max_attempts >= 1),
  run_after        timestamptz NOT NULL DEFAULT now(),
  lease_owner      text,
  lease_expires_at timestamptz,
  idempotency_key  text UNIQUE CHECK (idempotency_key <> ''),
  last_error       text,
  created_at       timestamptz NOT NULL DEFAULT now(),
  updated_at       timestamptz NOT NULL DEFAULT now(),
  started_at       timestamptz,
  finished_at      timestamptz,
  CHECK ((state = 'running') = (lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL))
);
CREATE INDEX job_claimable_idx ON job (priority DESC, run_after, id) WHERE state IN ('queued', 'failed');
CREATE INDEX job_lease_idx ON job (lease_expires_at) WHERE state = 'running';
CREATE INDEX job_kind_state_idx ON job (kind, state);
