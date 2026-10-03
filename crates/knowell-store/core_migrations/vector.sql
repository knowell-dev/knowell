CREATE EXTENSION IF NOT EXISTS vector;

-- One vector per (profile, prepared input). The column has no fixed
-- dimension so profiles of different sizes share the table; every profile
-- gets its own partial HNSW index on `embedding::halfvec(<dimensions>)`
-- WHERE profile_id = '<id>', created when the profile is registered, and
-- similarity queries repeat exactly that expression and predicate. The cast
-- in the index also rejects vectors of the wrong dimension on insert.
CREATE TABLE IF NOT EXISTS embedding (
  profile_id          uuid NOT NULL REFERENCES embedding_profile (id) ON DELETE CASCADE,
  prepared_input_hash bytea NOT NULL CHECK (octet_length(prepared_input_hash) = 32),
  embedding           halfvec NOT NULL,
  created_at          timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (profile_id, prepared_input_hash)
);
