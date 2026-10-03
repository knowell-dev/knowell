-- Durable MCP tool usage: calls, errors, returned tokens and a latency
-- histogram, summed per organization, UTC hour, tool and agent label.
--
-- Engines buffer calls in memory and add them here every few seconds; the
-- additions are commutative sums, so several engines (hub replicas) can flush
-- into the same rows. `hour` is the start of a UTC hour (the store truncates
-- it). `latency_buckets` follows knowell_store::usage::latency_bucket.
CREATE TABLE tool_usage_hour (
  organization_id uuid NOT NULL REFERENCES organization (id) ON DELETE CASCADE,
  hour            timestamptz NOT NULL,
  tool            text NOT NULL CHECK (tool ~ '^[a-z][a-z0-9_]{0,63}$'),
  agent           text NOT NULL CHECK (agent ~ '^[A-Za-z0-9._-]{1,64}$'),
  calls           bigint NOT NULL CHECK (calls >= 1),
  errors          bigint NOT NULL CHECK (errors >= 0 AND errors <= calls),
  tokens_returned bigint NOT NULL CHECK (tokens_returned >= 0),
  latency_ms_sum  bigint NOT NULL CHECK (latency_ms_sum >= 0),
  latency_buckets bigint[] NOT NULL CHECK (cardinality(latency_buckets) = 96),
  last_call_at    timestamptz NOT NULL,
  PRIMARY KEY (organization_id, hour, tool, agent)
);
CREATE INDEX tool_usage_hour_last_call_idx ON tool_usage_hour (organization_id, last_call_at DESC);
