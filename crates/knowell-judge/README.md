# knowell-judge

Optional rerank and query-classification providers for Knowell. Reranking is
**off by default** and applies only to the short list left after hybrid
retrieval and fusion; it is kept only if evaluation shows it helps. The
[`LexicalOverlapReranker`] is the offline baseline a real reranker must beat.

## API

```rust
#[async_trait]
pub trait Reranker: Send + Sync {
    fn descriptor(&self) -> Descriptor;           // provider, model, version
    async fn rerank(&self, policy: DataPolicy, query: &str,
                    candidates: &[Candidate], top_n: usize)
        -> Result<Vec<Scored>, JudgeError>;       // best first, ties by input order
    fn usage(&self) -> Usage;                     // cumulative requests/documents/tokens/search_units
}

#[async_trait]
pub trait QueryClassifier: Send + Sync {
    fn descriptor(&self) -> Descriptor;
    async fn classify(&self, policy: DataPolicy, query: &str, labels: &[String])
        -> Result<Vec<(String, f32)>, JudgeError>;
}
```

`Candidate { id, text }`, `Scored { id, score }`, `DataPolicy { local_only }`.
Scores are only comparable within one call; `descriptor()` makes them
attributable to a provider, model and pinned version.

## Providers

| Type | Endpoint | Local-only allowed |
|---|---|---|
| `TeiReranker` | `POST {base}/rerank` (TEI-style) | yes, when the host is loopback or `assume_local = Some(true)` |
| `VoyageReranker` | `POST https://api.voyageai.com/v1/rerank` | never |
| `CohereReranker` | `POST https://api.cohere.com/v2/rerank` | never |
| `LexicalOverlapReranker` | none (in-process) | always |

There is no official OpenAI rerank API; "OpenAI-compatible" servers that
expose `/rerank` (vLLM, llama.cpp server, LM Studio, TEI) use the TEI shape,
which is what `TeiReranker` speaks. Point it at a TEI instance running, for
example, a Qwen3-Reranker or bge-reranker model.

### Verified API facts (checked 2026-10-02)

* **TEI** ([docs](https://huggingface.github.io/text-embeddings-inference/)):
  request `{query, texts, raw_scores, return_text, truncate,
  truncation_direction}`; response is a bare array `[{index, score}]`;
  optional `Authorization: Bearer` when the server runs with `--api-key`;
  errors are JSON `{error, error_type}`. No model field in the request.
  The fetched page did not state the server's batch limit; `TeiConfig`
  defaults `max_documents` to a conservative 32 (TEI's
  `--max-client-batch-size` is believed to default to 32, unverified) -
  set it to your server's value.
* **Voyage** ([docs](https://docs.voyageai.com/reference/reranker-api)):
  request `{query, documents, model, top_k, return_documents, truncation}`
  (`truncation` defaults to true); response `{object, data: [{index,
  relevance_score}], model, usage: {total_tokens}}`; `Authorization: Bearer`;
  at most 1000 documents; combined query+document token limit depends on the
  model (32 000 for `rerank-2.5`/`rerank-2.5-lite`); recommended models
  `rerank-2.5`, `rerank-2.5-lite`.
* **Cohere** ([docs](https://docs.cohere.com/reference/rerank)): v2 request
  `{model, query, documents, top_n, max_tokens_per_doc (default 4096),
  priority}`; response `{results: [{index, relevance_score}], meta:
  {billed_units: {search_units}}}`; `Authorization: Bearer`; no more than
  1000 documents per request is recommended.

## Limits, truncation, resilience (`HttpOptions`)

| Setting | Default | Behaviour |
|---|---|---|
| `max_documents` | 1000 (TEI: 32) | More candidates is `TooManyCandidates`; never split or dropped silently, because scores of separate calls are not comparable |
| `max_document_chars` | 8000 | Each document is cut to this many Unicode characters (char boundary) before sending; providers also truncate server-side (`truncate`/`truncation`/`max_tokens_per_doc`) |
| `max_query_chars` | 2000 | Same for the query |
| `timeout` / `connect_timeout` | 30 s / 5 s | Expiry is `JudgeError::Timeout` |
| `max_retries` | 2 | Retries on HTTP 429 and 5xx only, exponential backoff 200 ms doubling to 5 s (no jitter, deterministic) |
| `max_retry_after` | 30 s | `Retry-After` (delta-seconds) is honoured as a minimum wait; a longer request ends the call with `RateLimited` instead of stalling. The HTTP-date form is ignored |
| `max_concurrency` | 4 | In-flight requests per provider instance (semaphore) |

Other statuses map to `Unauthorized` (401/403) or `Http { status, message }`
without retry. Responses with out-of-range or duplicate indexes, non-finite
scores, no results, or invalid JSON are `InvalidResponse`.

Usage: `Reranker::usage()` returns cumulative HTTP requests (retries
included), documents sent, Voyage `total_tokens`, and Cohere billed
`search_units`.

## Data policy

Every call takes a `DataPolicy`. With `local_only: true`, Voyage and Cohere
fail with `JudgeError::PolicyRefused` before anything is sent; `TeiReranker`
runs only against a loopback endpoint (or one the operator explicitly marks
local with `assume_local`, which the library cannot verify).

## Security

* API keys are `secrecy::SecretString`; configs and providers have
  redacting `Debug`.
* Keys travel only in the `Authorization` header (marked sensitive). Base URLs
  with credentials, query strings or fragments are rejected; cloud providers
  require `https` except for loopback hosts.
* Provider error bodies and transport messages are passed through
  `knowell_secrets::Masker` (with the configured key) and truncated to 300
  characters before they enter an error, so a server that echoes the key
  back does not leak it. Tests cover this.

## Testing

`cargo test -p knowell-judge`. Provider tests use `wiremock` and fake keys
built at runtime; no real API is called.
