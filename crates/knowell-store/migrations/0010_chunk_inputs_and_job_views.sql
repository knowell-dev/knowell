-- Per-path prepared embedding inputs, and jobs attributed to one view.
--
-- Revert (manual, destroys data):
--   DROP TABLE chunk_input;
--   DROP INDEX job_view_idx;
--   ALTER TABLE job DROP COLUMN view_id;
--
-- chunk_input: the prepared embedding input of a chunk includes project and
-- path context, so it belongs to a file version (one path's content over a
-- generation interval), not to the content alone. Content-level `chunk` rows
-- keep lines, bytes, kind and symbol path for every path holding the
-- content; `chunk.prepared_input_hash` is the input of whichever path wrote
-- the rows first and is only consulted for data indexed before this
-- migration.
--
-- A chunk_input row is derived data, a function of the project, the path,
-- the content, the parser version and the chunking settings. It is therefore
-- not generation-fenced: it may be written while its generation builds or
-- later (a post-activation embedding stage backfilling older rows), and it
-- disappears with its file version (rolled back with a failed generation,
-- pruned with history) through the foreign key.
CREATE TABLE chunk_input (
  view_id             uuid NOT NULL,
  path                text NOT NULL,
  file_valid_from     bigint NOT NULL,
  parser_version      text NOT NULL CHECK (parser_version <> ''),
  ordinal             integer NOT NULL CHECK (ordinal >= 0),
  prepared_input_hash bytea NOT NULL CHECK (octet_length(prepared_input_hash) = 32),
  -- Whether the content policy lets this chunk be embedded (generated files
  -- are chunked but, by default, not embedded).
  embed               boolean NOT NULL,
  PRIMARY KEY (view_id, path, file_valid_from, parser_version, ordinal),
  FOREIGN KEY (view_id, path, file_valid_from)
    REFERENCES file_version (view_id, path, valid_from) ON DELETE CASCADE
);
-- Vector hits are located through their prepared input.
CREATE INDEX chunk_input_prepared_idx ON chunk_input (prepared_input_hash);

-- A job of one view (`enqueue_scoped` with a view scope records it), so a
-- worker can claim only the jobs of the views it serves (`claim_scoped`).
-- Deleting the view deletes its jobs: their payloads point at nothing.
ALTER TABLE job ADD COLUMN view_id uuid REFERENCES view (id) ON DELETE CASCADE;
CREATE INDEX job_view_idx ON job (view_id) WHERE view_id IS NOT NULL;
