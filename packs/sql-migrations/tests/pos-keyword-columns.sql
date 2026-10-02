-- Keyword-named columns and column CHECK constraints (the tree-sitter SQL
-- grammar loses `active` here).
CREATE TABLE tariffs (
    id       varchar(64) PRIMARY KEY,
    interval varchar(8)  NOT NULL CHECK (interval IN ('month', 'year')),
    active   boolean     NOT NULL DEFAULT true,
    CONSTRAINT tariffs_interval_check CHECK (interval <> '')
);
ALTER TABLE tariffs RENAME COLUMN active TO enabled;
