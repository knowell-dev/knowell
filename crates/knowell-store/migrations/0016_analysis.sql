-- Persist prepared, source-free compiler analysis independently of jobs.
-- Imports are explicitly selected for a matching source revision; an import
-- is never inherited by a later generation merely because it exists.
CREATE TABLE scip_import (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  view_id         uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  source_revision text NOT NULL CHECK (source_revision ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  artifact_hash   bytea NOT NULL CHECK (octet_length(artifact_hash) = 32),
  payload         jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (organization_id, view_id, source_revision, artifact_hash)
);

-- No public API updates imports. Enforce that property at the durable boundary
-- as well, while allowing deletion through the owning view's retention policy.
CREATE FUNCTION knowell_scip_import_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'scip imports are immutable' USING ERRCODE = '55000';
END;
$$;
CREATE TRIGGER scip_import_immutable BEFORE UPDATE ON scip_import
FOR EACH ROW EXECUTE FUNCTION knowell_scip_import_immutable();

-- Coverage describes an exact file in an exact analysis generation. It is not
-- a validity interval: unchanged text does not prove that a new build has the
-- same dependencies, feature flags, compiler or analysis inputs.
CREATE TABLE analysis_coverage (
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  view_id         uuid NOT NULL,
  generation      bigint NOT NULL CHECK (generation > 0),
  path            text NOT NULL CHECK (path <> ''),
  content_hash    bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  provider        text NOT NULL CHECK (provider IN ('syntax', 'scip')),
  details         jsonb NOT NULL CHECK (jsonb_typeof(details) = 'object'),
  PRIMARY KEY (organization_id, view_id, generation, path, provider),
  FOREIGN KEY (view_id, generation)
    REFERENCES view_generation (view_id, generation) ON DELETE CASCADE
);

-- Syntax and compiler occurrences must survive each other's per-file retries.
-- Existing rows retain their original syntactic provenance.
ALTER TABLE occurrence ADD COLUMN origin text NOT NULL DEFAULT 'syntax'
  CHECK (origin = 'syntax' OR (left(origin, 5) = 'scip:'
    AND octet_length(origin) BETWEEN 6 AND 1024));
CREATE INDEX occurrence_origin_open_idx ON occurrence (view_id, origin)
  WHERE valid_to IS NULL;

-- Support bounded inspection without scanning a symbol's whole reference list.
CREATE INDEX occurrence_symbol_page_idx
  ON occurrence (symbol_id, view_id, path COLLATE "C", start_line, end_line, role, id);