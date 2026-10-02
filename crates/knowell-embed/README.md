# knowell-embed

Embedding providers for Knowell: profiles, input preparation, batching, rate limits,
retries, budgets and cache keys. Nothing here talks to a real API in tests; every provider
is exercised against a local mock server with fake keys.

## Providers

| Type | Endpoint | Auth | Notes |
|---|---|---|---|
| `GeminiEmbedder` | `POST {base}/v1beta/models/{model}:batchEmbedContents` | `x-goog-api-key` header | Default model `gemini-embedding-2`, dimensions 128-3072 |
| `OpenAiCompatibleEmbedder` | `POST {base}/v1/embeddings` | optional `Authorization: Bearer` | `dimensions` sent only when `send_dimensions` is set |
| `OllamaEmbedder` | `POST {base}/api/embed` | none | `truncate: false`, so over-long input fails instead of being cut |
| `FakeEmbedder` | none | none | Deterministic feature hashing of word/char n-grams; weak semantics, identical on every platform; for tests and CI |

`AnyEmbedder` wraps the four for runtime selection (the `Embedder` trait uses
`impl Future` returns and is not object safe).

## Profiles and cache keys

`EmbeddingProfile { provider_kind, model, dimensions, input_format_version }` identifies a
vector space. `profile_key()` is a stable BLAKE3 hash of those fields. Vectors from
different profiles must never be compared; `Embedding::cosine` returns `None` when
dimensions differ, but the real guard is to store and query by profile key.
`cache_key(profile, prepared_input_hash)` combines the profile with the hash of the
prepared input (`prepared_document_hash`), so a cached vector is never served for another
profile. Bump `input_format_version` whenever input preparation changes.

## Input preparation (Gemini Embedding 2)

There is no `task_type`; the task is part of the text:

- query: `task: code retrieval | query: {text}`
- document: `title: {title} | text: {text}` (`title: none` when there is no title)

Every chunk is its own request entry with a single part: several parts inside one content
are merged into one embedding. All vectors are L2-normalised by this crate, whatever the
dimensionality. `truncate_and_normalize(&Embedding, new_dims)` shortens a vector without an
API call; valid only for Matryoshka-trained models (Gemini Embedding 2 is) and the result
needs a new profile.

## Rate limits, retries, budget

- **Concurrency**: a semaphore per provider instance (`RequestLimits::max_concurrency`).
  Calls are sent sequentially batch by batch; run several calls concurrently to use the limit.
- **Token buckets**: optional requests/minute and (estimated) tokens/minute, refilled
  continuously; the bucket capacity is one minute of quota.
- **Batching**: at most 100 entries (Gemini) and `max_batch_tokens` estimated tokens per
  request. Token estimates are conservative (1 token per 3 characters). An input above
  `max_input_tokens` (8192 for Gemini) is refused with `InputTooLong`, never truncated.
- **Retries**: 429, 408, 5xx, timeouts and connection failures, exponential backoff with
  jitter in `[delay/2, delay]`. `Retry-After` (delay-seconds) is honoured; if it exceeds
  `max_retry_after` the call fails with `RateLimited` so the caller can reschedule.
  401/403 and other 4xx are never retried. Redirects are not followed (they would forward
  the key header to another host).
- **Budget**: `Budget::new(max_tokens, max_usd, usd_per_million_tokens)`, shared by clones.
  A call reserves its estimated tokens up front and is refused whole with
  `BudgetExceeded` before anything is sent; actual (provider-reported, else estimated)
  tokens are charged on completion, and unused reservation is released on failure.
- **Usage**: every `*_with_usage` call returns `Usage { input_tokens, tokens_estimated,
  requests, retries, latency }`.

## Security

The key is a `SecretString` supplied by the caller. It goes only into a sensitive request
header. Provider error bodies and transport errors are masked with
`knowell_secrets::Masker` and cut to 300 characters before they enter an error; `Debug`
output of embedders and errors never contains it. Tests assert this for 401, 500 with an
echoing body, 400, malformed JSON and a refused connection.

## Verified against the official docs (2026-10-02)

- https://ai.google.dev/gemini-api/docs/embeddings
- https://ai.google.dev/api/embeddings
- https://ai.google.dev/gemini-api/docs/batch-api

Confirmed: model id `gemini-embedding-2`; no `task_type` for this model, tasks via text
prefixes (including `task: code retrieval | query: ...` and `title: {title} | text: ...`
with `title: none`); `output_dimensionality` 128-3072, recommended 768/1536/3072; several
inputs in one content produce one aggregated embedding, separate `Content` objects give
separate embeddings; 8192 input tokens; `x-goog-api-key` header; `batchEmbedContents`
takes `requests[]` of `{model: "models/gemini-embedding-2", content: {parts: [...]}}` and
returns `embeddings[].values` plus `usageMetadata.promptTokenCount`.

Differences and caveats:

- The docs say Gemini Embedding 2 **auto-normalises** truncated dimensions. We normalise
  anyway (idempotent) so that all providers behave the same.
- The 100-entry cap is **our** conservative limit; the reference states no maximum.
- `outputDimensionality` is sent as a top-level field of each request entry. The guide's
  curl example uses a top-level `output_dimensionality`, while the API reference marks the
  top-level fields as deprecated in favour of an `embedContentConfig` object. Not
  verified against the live service; switch in `gemini.rs` if the top-level form is ever
  dropped.

### Batch API (deferred)

The Batch API (50% of the standard price, 24 h target turnaround, 48 h expiry, inline
requests under 20 MB or JSONL files up to 2 GB through the File API, polled job states,
results kept 6 weeks) exists for embeddings (`models.asyncBatchEmbedContent`), but the
docs available to us do not give a verifiable REST request/response shape for the job,
file upload and result retrieval flow. It also needs durable job state in the indexing
queue. It is deferred until the flow can be verified; synchronous `batchEmbedContents`
covers initial indexing meanwhile.
