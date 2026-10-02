-- Embedding profiles, embeddings and per-profile index generations.
--
-- Revert (manual, destroys data):
--   DROP TABLE index_generation, embedding, embedding_profile;
--   DROP FUNCTION knowell_embedding_profile_immutable();
--   (dropping `embedding` also drops the per-profile HNSW indexes)

-- Provider, model, dimensions and input format together define a profile.
-- Changing any of them means a new profile; rows are never updated.
CREATE TABLE embedding_profile (
  id                   uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  organization_id      uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  name                 text NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  provider             text NOT NULL CHECK (provider <> ''),
  model                text NOT NULL CHECK (model <> ''),
  -- halfvec HNSW indexes support at most 4000 dimensions.
  dimensions           integer NOT NULL CHECK (dimensions BETWEEN 1 AND 4000),
  input_format_version text NOT NULL CHECK (input_format_version <> ''),
  created_at           timestamptz NOT NULL DEFAULT now(),
  UNIQUE (organization_id, name),
  UNIQUE (organization_id, provider, model, dimensions, input_format_version)
);

CREATE FUNCTION knowell_embedding_profile_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'embedding profiles are immutable; register a new profile instead'
    USING ERRCODE = 'restrict_violation';
END
$$;

CREATE TRIGGER embedding_profile_immutable
  BEFORE UPDATE ON embedding_profile
  FOR EACH ROW EXECUTE FUNCTION knowell_embedding_profile_immutable();

-- One vector per (profile, prepared input). The column has no fixed
-- dimension so profiles of different sizes share the table; every profile
-- gets its own partial HNSW index on `embedding::halfvec(<dimensions>)`
-- WHERE profile_id = '<id>', created when the profile is registered, and
-- similarity queries repeat exactly that expression and predicate. The cast
-- in the index also rejects vectors of the wrong dimension on insert.
CREATE TABLE embedding (
  profile_id          uuid NOT NULL REFERENCES embedding_profile (id) ON DELETE CASCADE,
  prepared_input_hash bytea NOT NULL CHECK (octet_length(prepared_input_hash) = 32),
  embedding           halfvec NOT NULL,
  created_at          timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (profile_id, prepared_input_hash)
);

-- Which view generation the vectors of a profile cover, blue-green style.
CREATE TABLE index_generation (
  id              uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  view_id         uuid NOT NULL,
  profile_id      uuid NOT NULL REFERENCES embedding_profile (id) ON DELETE CASCADE,
  view_generation bigint NOT NULL,
  state           generation_state NOT NULL DEFAULT 'building',
  chunk_count     bigint NOT NULL DEFAULT 0 CHECK (chunk_count >= 0),
  embedded_count  bigint NOT NULL DEFAULT 0 CHECK (embedded_count >= 0),
  error           text,
  created_at      timestamptz NOT NULL DEFAULT now(),
  activated_at    timestamptz,
  finished_at     timestamptz,
  UNIQUE (view_id, profile_id, view_generation),
  FOREIGN KEY (view_id, view_generation) REFERENCES view_generation (view_id, generation) ON DELETE CASCADE
);
CREATE UNIQUE INDEX index_generation_one_active ON index_generation (view_id, profile_id) WHERE state = 'active';
