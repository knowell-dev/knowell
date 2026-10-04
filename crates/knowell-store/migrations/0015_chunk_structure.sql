-- Add source-only hierarchy without rewriting historical chunks or vectors.
-- Absence means the indexed analysis predates this metadata; it does not mean
-- that an embedding input is an exact, contiguous source excerpt.
-- Legacy counts remain unknown; a migration must not load all source bodies.
ALTER TABLE content ADD COLUMN redacted_line_count bigint
  CHECK (redacted_line_count >= 0 AND redacted_line_count <= 4294967295);

CREATE TABLE chunk_structure (
  organization_id       uuid NOT NULL,
  content_hash          bytea NOT NULL,
  parser_version        text NOT NULL,
  ordinal               integer NOT NULL,
  parent_ordinal        integer,
  declaration_start_line integer,
  declaration_end_line   integer,
  declaration_start_byte bigint,
  declaration_end_byte   bigint,
  enclosing_start_line   integer,
  enclosing_end_line     integer,
  enclosing_start_byte   bigint,
  enclosing_end_byte     bigint,
  source_exact           boolean NOT NULL,
  PRIMARY KEY (organization_id, content_hash, parser_version, ordinal),
  FOREIGN KEY (organization_id, content_hash, parser_version, ordinal)
    REFERENCES chunk (organization_id, content_hash, parser_version, ordinal)
    ON DELETE CASCADE,
  FOREIGN KEY (organization_id, content_hash, parser_version, parent_ordinal)
    REFERENCES chunk (organization_id, content_hash, parser_version, ordinal)
    ON DELETE CASCADE,
  CHECK (parent_ordinal IS NULL OR (parent_ordinal >= 0 AND parent_ordinal < ordinal)),
  CHECK ((declaration_start_line IS NULL AND declaration_end_line IS NULL
          AND declaration_start_byte IS NULL AND declaration_end_byte IS NULL)
      OR (declaration_start_line IS NOT NULL AND declaration_end_line IS NOT NULL
          AND declaration_start_byte IS NOT NULL AND declaration_end_byte IS NOT NULL
          AND declaration_start_line >= 1 AND declaration_end_line >= declaration_start_line
          AND declaration_start_byte >= 0 AND declaration_end_byte >= declaration_start_byte)),
  CHECK ((enclosing_start_line IS NULL AND enclosing_end_line IS NULL
          AND enclosing_start_byte IS NULL AND enclosing_end_byte IS NULL)
      OR (enclosing_start_line IS NOT NULL AND enclosing_end_line IS NOT NULL
          AND enclosing_start_byte IS NOT NULL AND enclosing_end_byte IS NOT NULL
          AND enclosing_start_line >= 1 AND enclosing_end_line >= enclosing_start_line
          AND enclosing_start_byte >= 0 AND enclosing_end_byte >= enclosing_start_byte))
);
