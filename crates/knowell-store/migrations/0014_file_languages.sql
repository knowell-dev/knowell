-- Language belongs to the path's version, not to shared content bytes.
-- The Rust detector backfills every historical interval under the migration lock.
-- Manual revert: drop file_version_language_pending_idx, then drop the language
-- and language_detection_version columns. Stop writers before either direction.
ALTER TABLE file_version
  ADD COLUMN language text,
  ADD COLUMN language_detection_version smallint NOT NULL DEFAULT 0
    CHECK (language_detection_version >= 0);

CREATE INDEX file_version_language_pending_idx
  ON file_version (view_id, path, valid_from)
  WHERE language_detection_version = 0;
