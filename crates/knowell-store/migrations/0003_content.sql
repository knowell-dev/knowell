-- Content, chunks, file versions, symbols and occurrences.
--
-- Revert (manual, destroys data):
--   DROP TABLE occurrence, symbol, file_version, chunk, content;
--
-- Hashes are BLAKE3 digests (knowell_core::ContentHash) stored as 32 raw
-- bytes; use encode(hash, 'hex') in psql to read them.
--
-- Generation-scoped tables (file_version, occurrence, and edge/contract in
-- 0004) store validity intervals instead of one copy per generation: a row
-- is part of generation g of its view when
--   valid_from <= g AND (valid_to IS NULL OR valid_to > g).
-- A new generation only writes what changed; a failed generation's rows are
-- rolled back by deleting rows born in it and reopening rows it closed.

-- Content-addressed, redacted text. Scoped to one organization: content is
-- never shared across tenants, and redaction policy may differ per tenant.
CREATE TABLE content (
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  hash            bytea NOT NULL CHECK (octet_length(hash) = 32),
  size_bytes      bigint NOT NULL CHECK (size_bytes >= 0),
  language        text,
  -- Text after secret redaction. NULL when the content is not stored as text
  -- (binary, too large, or excluded by policy).
  redacted_text   text,
  created_at      timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (organization_id, hash)
);

-- A meaningful unit of a content blob, as cut by one parser version.
CREATE TABLE chunk (
  organization_id     uuid NOT NULL,
  content_hash        bytea NOT NULL,
  parser_version      text NOT NULL CHECK (parser_version <> ''),
  ordinal             integer NOT NULL CHECK (ordinal >= 0),
  start_line          integer NOT NULL CHECK (start_line >= 1),
  end_line            integer NOT NULL,
  start_byte          bigint NOT NULL CHECK (start_byte >= 0),
  end_byte            bigint NOT NULL,
  kind                text NOT NULL CHECK (kind <> ''),
  symbol_path         text,
  -- Hash of the prepared embedding input (content plus context header); the
  -- embedding cache key together with the embedding profile.
  prepared_input_hash bytea NOT NULL CHECK (octet_length(prepared_input_hash) = 32),
  created_at          timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (organization_id, content_hash, parser_version, ordinal),
  FOREIGN KEY (organization_id, content_hash) REFERENCES content (organization_id, hash) ON DELETE CASCADE,
  CHECK (end_line >= start_line),
  CHECK (end_byte >= start_byte)
);
CREATE INDEX chunk_prepared_input_idx ON chunk (organization_id, prepared_input_hash);

-- Which content a path has in a view, per generation interval.
CREATE TABLE file_version (
  view_id      uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  path         text NOT NULL CHECK (path <> '' AND path !~ '^/'),
  valid_from   bigint NOT NULL CHECK (valid_from > 0),
  valid_to     bigint CHECK (valid_to > valid_from),
  content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  -- Set when this version was created by a rename/move; history follows it.
  renamed_from text,
  PRIMARY KEY (view_id, path, valid_from)
);
CREATE UNIQUE INDEX file_version_open ON file_version (view_id, path) WHERE valid_to IS NULL;
CREATE INDEX file_version_closed_idx ON file_version (view_id, valid_to) WHERE valid_to IS NOT NULL;
CREATE INDEX file_version_born_idx ON file_version (view_id, valid_from);
CREATE INDEX file_version_content_idx ON file_version (content_hash, view_id);

-- Logical symbol identity: stable across content changes, renames and moves.
CREATE TABLE symbol (
  id             uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  project_id     uuid NOT NULL REFERENCES project (id) ON DELETE CASCADE,
  qualified_name text NOT NULL CHECK (qualified_name <> ''),
  kind           text NOT NULL CHECK (kind <> ''),
  created_at     timestamptz NOT NULL DEFAULT now(),
  updated_at     timestamptz NOT NULL DEFAULT now(),
  UNIQUE (project_id, kind, qualified_name)
);

-- Where a symbol is defined or referenced in a view, per generation interval.
CREATE TABLE occurrence (
  id           uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  symbol_id    uuid NOT NULL REFERENCES symbol (id) ON DELETE CASCADE,
  view_id      uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  path         text NOT NULL CHECK (path <> ''),
  content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  start_line   integer NOT NULL CHECK (start_line >= 1),
  end_line     integer NOT NULL,
  role         occurrence_role NOT NULL,
  valid_from   bigint NOT NULL CHECK (valid_from > 0),
  valid_to     bigint CHECK (valid_to > valid_from),
  CHECK (end_line >= start_line)
);
CREATE INDEX occurrence_symbol_idx ON occurrence (symbol_id, view_id);
CREATE INDEX occurrence_path_open_idx ON occurrence (view_id, path) WHERE valid_to IS NULL;
CREATE INDEX occurrence_closed_idx ON occurrence (view_id, valid_to) WHERE valid_to IS NOT NULL;
CREATE INDEX occurrence_born_idx ON occurrence (view_id, valid_from);
