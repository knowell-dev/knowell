-- Relation graph: evidence-carrying edges and cross-project contracts.
--
-- Revert (manual, destroys data):
--   DROP TABLE contract, edge;
--
-- Nodes are referenced as (kind, id, key) triples so one edge table can link
-- symbols, files, projects, contracts and names that are known only as text:
--   symbol   (symbol.id,    '')
--   file     (project.id,   repository path)
--   project  (project.id,   '')
--   contract (workspace.id, '<contract kind>:<key>')
--   name     (project.id,   unresolved or external name)
--
-- Both tables are generation-scoped (validity intervals, see 0003). `origin`
-- groups the rows one analysis step produced (by convention the repository
-- path of the analysed file) so a re-analysis replaces exactly those rows.

CREATE TABLE edge (
  id            uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  view_id       uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  origin        text NOT NULL CHECK (origin <> ''),
  from_kind     node_kind NOT NULL,
  from_id       uuid NOT NULL,
  from_key      text NOT NULL DEFAULT '',
  to_kind       node_kind NOT NULL,
  to_id         uuid NOT NULL,
  to_key        text NOT NULL DEFAULT '',
  kind          text NOT NULL CHECK (kind <> ''),
  -- Evidence type and resolution are separate on purpose; they are never
  -- folded into one confidence score.
  evidence_type evidence_type NOT NULL,
  resolution    edge_resolution NOT NULL,
  evidence      jsonb NOT NULL DEFAULT '{}'::jsonb,
  valid_from    bigint NOT NULL CHECK (valid_from > 0),
  valid_to      bigint CHECK (valid_to > valid_from),
  CHECK ((from_kind IN ('symbol', 'project')) = (from_key = '')),
  CHECK ((to_kind IN ('symbol', 'project')) = (to_key = ''))
);
CREATE INDEX edge_from_idx ON edge (from_kind, from_id, from_key);
CREATE INDEX edge_to_idx ON edge (to_kind, to_id, to_key);
CREATE INDEX edge_origin_open_idx ON edge (view_id, origin) WHERE valid_to IS NULL;
CREATE INDEX edge_closed_idx ON edge (view_id, valid_to) WHERE valid_to IS NOT NULL;
CREATE INDEX edge_born_idx ON edge (view_id, valid_from);

-- A project's participation in a contract: "project P produces topic K",
-- "project Q consumes endpoint GET /orders". Producers and consumers of the
-- same (kind, key) in a workspace form the cross-project link.
CREATE TABLE contract (
  id            uuid PRIMARY KEY DEFAULT knowell_uuidv7(),
  view_id       uuid NOT NULL REFERENCES view (id) ON DELETE CASCADE,
  origin        text NOT NULL CHECK (origin <> ''),
  kind          contract_kind NOT NULL,
  key           text NOT NULL CHECK (key <> ''),
  role          contract_role NOT NULL,
  symbol_id     uuid REFERENCES symbol (id) ON DELETE SET NULL,
  evidence_type evidence_type NOT NULL,
  evidence      jsonb NOT NULL DEFAULT '{}'::jsonb,
  valid_from    bigint NOT NULL CHECK (valid_from > 0),
  valid_to      bigint CHECK (valid_to > valid_from)
);
CREATE INDEX contract_key_idx ON contract (kind, key);
CREATE INDEX contract_origin_open_idx ON contract (view_id, origin) WHERE valid_to IS NULL;
CREATE INDEX contract_closed_idx ON contract (view_id, valid_to) WHERE valid_to IS NOT NULL;
CREATE INDEX contract_born_idx ON contract (view_id, valid_from);
