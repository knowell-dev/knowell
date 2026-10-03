#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Provider tests against a local mock server. No real API is ever called and
//! all keys are fake values built at runtime.

use std::time::{Duration, Instant};

use knowell_embed::{
    BatchLimits, Budget, DocumentInput, EmbedError, Embedder, GeminiConfig, GeminiEmbedder,
    OllamaConfig, OllamaEmbedder, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, RequestLimits,
    RetryPolicy,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const GEMINI_PATH: &str = "/v1beta/models/gemini-embedding-2:batchEmbedContents";

/// A fake key that is clearly not real, assembled at runtime.
fn fake_key() -> String {
    format!("AIzaSyFAKE{}-{}", "k".repeat(12), "canary")
}

fn secret() -> SecretString {
    SecretString::from(fake_key())
}

fn fast_limits() -> RequestLimits {
    RequestLimits {
        timeout: Duration::from_secs(5),
        max_concurrency: 2,
        requests_per_minute: None,
        tokens_per_minute: None,
        retry: RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            max_retry_after: Duration::from_secs(3),
        },
    }
}

fn gemini_config(server: &MockServer, dims: u32) -> GeminiConfig {
    GeminiConfig {
        base_url: Url::parse(&server.uri()).ok(),
        dimensions: dims,
        limits: fast_limits(),
        ..GeminiConfig::default()
    }
}

fn request_count(body: &Value) -> usize {
    body.get("requests")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// Gemini-shaped success response: one constant (non-unit) vector per entry.
fn gemini_ok(dims: usize) -> impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static {
    move |req: &Request| {
        let body: Value = req.body_json().unwrap();
        let n = request_count(&body);
        let embeddings: Vec<Value> = (0..n)
            .map(|i| json!({ "values": vec![(i + 1) as f32 * 2.0; dims] }))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": embeddings,
            "usageMetadata": { "promptTokenCount": 7 * n },
        }))
    }
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.body_json().unwrap())
        .collect()
}

fn assert_no_key(text: &str) {
    assert!(!text.contains(&fake_key()), "key leaked: {text}");
    assert!(!text.contains("AIzaSyFAKE"), "key fragment leaked: {text}");
}

// ---------------------------------------------------------------- Gemini

#[tokio::test]
async fn gemini_preserves_each_live_evaluation_dimension_and_rejects_wrong_sizes() {
    for dimensions in [768, 1536, 3072] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(GEMINI_PATH))
            .respond_with(gemini_ok(dimensions))
            .mount(&server)
            .await;
        let embedder =
            GeminiEmbedder::new(secret(), gemini_config(&server, dimensions as u32)).unwrap();
        assert_eq!(
            embedder
                .embed_query("cancel subscription")
                .await
                .unwrap()
                .dimensions(),
            dimensions
        );
        assert_eq!(
            bodies(&server).await[0]["requests"][0]["outputDimensionality"],
            dimensions
        );
        server.reset().await;
        Mock::given(method("POST"))
            .and(path(GEMINI_PATH))
            .respond_with(gemini_ok(dimensions - 1))
            .mount(&server)
            .await;
        assert!(matches!(
            embedder.embed_query("cancel subscription").await,
            Err(EmbedError::Response { .. })
        ));
    }
}

#[tokio::test]
async fn gemini_request_shape_prefixes_and_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .and(header("x-goog-api-key", fake_key().as_str()))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;

    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let docs = [
        DocumentInput::with_title("src/lib.rs", "fn main() {}"),
        DocumentInput::new("struct A;"),
        DocumentInput::new("struct B;"),
    ];
    let out = embedder.embed_documents_with_usage(&docs).await.unwrap();

    let sent = bodies(&server).await;
    assert_eq!(sent.len(), 1);
    let requests = sent[0]["requests"].as_array().unwrap();
    // One request entry per chunk, each with exactly one part.
    assert_eq!(requests.len(), 3);
    let expected_texts = [
        "title: src/lib.rs | text: fn main() {}",
        "title: none | text: struct A;",
        "title: none | text: struct B;",
    ];
    for (entry, text) in requests.iter().zip(expected_texts) {
        assert_eq!(entry["model"], "models/gemini-embedding-2");
        assert_eq!(entry["content"]["parts"].as_array().unwrap().len(), 1);
        assert_eq!(entry["content"]["parts"][0]["text"], text);
        assert_eq!(entry["outputDimensionality"], 128);
        assert!(entry.get("taskType").is_none() && entry.get("task_type").is_none());
    }

    // Key only in the header, never in the URL.
    let received = server.received_requests().await.unwrap();
    assert!(received[0].url.query().is_none());
    assert_no_key(received[0].url.as_str());

    // Vectors are normalised and in order; usage comes from the provider.
    assert_eq!(out.embeddings.len(), 3);
    for e in &out.embeddings {
        assert_eq!(e.dimensions(), 128);
        let norm: f32 = e.as_slice().iter().map(|v| v * v).sum();
        assert!((norm - 1.0).abs() < 1e-4);
    }
    assert_eq!(out.usage.input_tokens, 21);
    assert!(!out.usage.tokens_estimated);
    assert_eq!(out.usage.requests, 1);
    assert_eq!(out.usage.retries, 0);
}

#[tokio::test]
async fn gemini_query_uses_code_retrieval_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .respond_with(gemini_ok(768))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 768)).unwrap();
    let q = embedder
        .embed_query("where is the retry loop")
        .await
        .unwrap();
    assert_eq!(q.dimensions(), 768);
    let sent = bodies(&server).await;
    assert_eq!(
        sent[0]["requests"][0]["content"]["parts"][0]["text"],
        "task: code retrieval | query: where is the retry loop"
    );
    assert_eq!(sent[0]["requests"][0]["outputDimensionality"], 768);
    assert_eq!(embedder.profile().model, "gemini-embedding-2");
    assert_eq!(embedder.profile().dimensions, 768);
}

#[tokio::test]
async fn gemini_splits_requests_into_batches_of_100() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let docs: Vec<_> = (0..250)
        .map(|i| DocumentInput::new(format!("chunk {i}")))
        .collect();
    let out = embedder.embed_documents_with_usage(&docs).await.unwrap();
    assert_eq!(out.embeddings.len(), 250);
    assert_eq!(out.usage.requests, 3);
    let sizes: Vec<usize> = bodies(&server).await.iter().map(request_count).collect();
    assert_eq!(sizes, vec![100, 100, 50]);
}

#[tokio::test]
async fn gemini_splits_by_token_estimate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;
    let config = GeminiConfig {
        batch: BatchLimits {
            max_entries: 100,
            max_batch_tokens: 1000,
            max_input_tokens: 600,
        },
        ..gemini_config(&server, 128)
    };
    let embedder = GeminiEmbedder::new(secret(), config).unwrap();
    // Each text is about 400 estimated tokens: two never fit one request.
    let docs: Vec<_> = (0..3)
        .map(|_| DocumentInput::new("x".repeat(1200)))
        .collect();
    embedder.embed_documents(&docs).await.unwrap();
    let sizes: Vec<usize> = bodies(&server).await.iter().map(request_count).collect();
    assert_eq!(sizes, vec![2, 1]);
}

#[tokio::test]
async fn gemini_refuses_oversized_input_without_sending() {
    let server = MockServer::start().await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let huge = DocumentInput::new("x".repeat(100_000));
    let err = embedder.embed_documents(&[huge]).await.unwrap_err();
    assert!(
        matches!(err, EmbedError::InputTooLong { index: 0, .. }),
        "{err}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn gemini_retries_429_honouring_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("slow down"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let started = Instant::now();
    let out = embedder
        .embed_documents_with_usage(&[DocumentInput::new("a")])
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(990),
        "retry-after was not honoured: {:?}",
        started.elapsed()
    );
    assert_eq!(out.usage.retries, 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn gemini_gives_up_when_retry_after_exceeds_the_cap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            EmbedError::RateLimited {
                status: 429,
                retry_after_secs: Some(3600),
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn gemini_retries_5xx_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let out = embedder
        .embed_documents_with_usage(&[DocumentInput::new("a")])
        .await
        .unwrap();
    assert_eq!(out.usage.retries, 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn gemini_stops_after_max_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Http { status: 500, .. }), "{err}");
    // First attempt plus three retries.
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn gemini_does_not_retry_client_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Http { status: 400, .. }), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn gemini_times_out() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "embeddings": [] }))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    let mut config = gemini_config(&server, 128);
    config.limits.timeout = Duration::from_millis(150);
    config.limits.retry.max_retries = 1;
    let embedder = GeminiEmbedder::new(secret(), config).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(err, EmbedError::Timeout { attempts: 2, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn gemini_budget_refuses_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;
    let budget = Budget::new(Some(50), None, 0.0).unwrap();
    let config = GeminiConfig {
        budget: Some(budget.clone()),
        ..gemini_config(&server, 128)
    };
    let embedder = GeminiEmbedder::new(secret(), config).unwrap();
    let docs = [DocumentInput::new("y".repeat(900))];
    let err = embedder.embed_documents(&docs).await.unwrap_err();
    assert!(matches!(err, EmbedError::BudgetExceeded(_)), "{err}");
    assert!(err.to_string().contains("budget"));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(budget.spent_tokens(), 0);

    // A call that fits is charged with the provider-reported tokens (7).
    embedder
        .embed_documents(&[DocumentInput::new("small")])
        .await
        .unwrap();
    assert_eq!(budget.spent_tokens(), 7);
}

#[tokio::test]
async fn gemini_rejects_wrong_dimensions_and_counts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(gemini_ok(64)) // profile says 128
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Response { .. }), "{err}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "embeddings": [] })))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Response { .. }), "{err}");
}

#[tokio::test]
async fn gemini_rejects_malformed_json_without_echoing_secrets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("not json {}", fake_key())),
        )
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Response { .. }));
    assert_no_key(&err.to_string());
    assert_no_key(&format!("{err:?}"));
}

#[test]
fn gemini_validates_configuration() {
    let make = |dims: u32| {
        GeminiEmbedder::new(
            secret(),
            GeminiConfig {
                dimensions: dims,
                ..GeminiConfig::default()
            },
        )
    };
    assert!(make(127).is_err());
    assert!(make(3073).is_err());
    assert!(make(128).is_ok());
    assert!(make(3072).is_ok());
    let bad_model = GeminiConfig {
        model: "a/b?key=1".into(),
        ..GeminiConfig::default()
    };
    assert!(GeminiEmbedder::new(secret(), bad_model).is_err());
    let too_many = GeminiConfig {
        batch: BatchLimits {
            max_entries: 101,
            max_batch_tokens: 50_000,
            max_input_tokens: 8_192,
        },
        ..GeminiConfig::default()
    };
    assert!(GeminiEmbedder::new(secret(), too_many).is_err());
    let with_creds = GeminiConfig {
        base_url: Url::parse("https://user:pw@example.com").ok(),
        ..GeminiConfig::default()
    };
    assert!(GeminiEmbedder::new(secret(), with_creds).is_err());
}

#[tokio::test]
async fn gemini_errors_and_debug_never_contain_the_key() {
    // 401
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(format!("invalid key {}", fake_key())),
        )
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    assert_no_key(&format!("{embedder:?}"));
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Auth { status: 401, .. }), "{err}");
    assert_no_key(&err.to_string());
    assert_no_key(&format!("{err:?}"));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "401 must not be retried"
    );

    // 500 with a body that echoes the key (and is long).
    let server = MockServer::start().await;
    let echo = format!("{} {} {}", "pad".repeat(50), fake_key(), "tail".repeat(200));
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string(echo))
        .mount(&server)
        .await;
    let mut config = gemini_config(&server, 128);
    config.limits.retry.max_retries = 0;
    let embedder = GeminiEmbedder::new(secret(), config).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    let text = err.to_string();
    assert_no_key(&text);
    assert_no_key(&format!("{err:?}"));
    assert!(text.contains("[REDACTED]"), "{text}");
    assert!(text.contains("truncated"), "{text}");
    assert!(text.len() < 600, "{}", text.len());

    // 400 echoing the key.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(format!("{{\"error\":\"{}\"}}", fake_key())),
        )
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert_no_key(&err.to_string());
}

#[tokio::test]
async fn gemini_network_error_never_contains_the_key() {
    // Reserve a port, then close it so connections are refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config = GeminiConfig {
        base_url: Url::parse(&format!("http://127.0.0.1:{port}")).ok(),
        dimensions: 128,
        limits: RequestLimits {
            retry: RetryPolicy {
                max_retries: 1,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(2),
                max_retry_after: Duration::from_secs(1),
            },
            ..fast_limits()
        },
        ..GeminiConfig::default()
    };
    let embedder = GeminiEmbedder::new(secret(), config).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(err, EmbedError::Network { attempts: 2, .. }),
        "{err}"
    );
    assert_no_key(&err.to_string());
    assert_no_key(&format!("{err:?}"));
}

#[tokio::test]
async fn gemini_does_not_follow_redirects() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", other.uri().as_str()))
        .mount(&server)
        .await;
    let embedder = GeminiEmbedder::new(secret(), gemini_config(&server, 128)).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Http { status: 307, .. }), "{err}");
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn gemini_works_under_configured_rate_limits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(gemini_ok(128))
        .mount(&server)
        .await;
    let mut config = gemini_config(&server, 128);
    // Throttling itself is covered by the token bucket unit tests; here we
    // check that configured limits do not get in the way of normal calls.
    config.limits.requests_per_minute = Some(120);
    config.limits.tokens_per_minute = Some(100_000);
    let embedder = GeminiEmbedder::new(secret(), config).unwrap();
    for _ in 0..3 {
        embedder
            .embed_documents(&[DocumentInput::new("a")])
            .await
            .unwrap();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

// ---------------------------------------------------------------- OpenAI

#[tokio::test]
async fn openai_request_shape_auth_and_index_ordering() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header(
            "authorization",
            format!("Bearer {}", fake_key()).as_str(),
        ))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().unwrap();
            let n = request_count(&body);
            // Return items in reverse order; `index` must be honoured.
            let data: Vec<Value> = (0..n)
                .rev()
                .map(|i| json!({ "index": i, "embedding": [(i + 1) as f32, 0.0, 0.0] }))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "data": data,
                "usage": { "prompt_tokens": 11, "total_tokens": 11 },
            }))
        })
        .mount(&server)
        .await;

    let mut config = OpenAiCompatibleConfig::new(
        Url::parse(&format!("{}/", server.uri())).unwrap(),
        "text-embedding-test",
        3,
    );
    config.send_dimensions = true;
    config.limits = fast_limits();
    let embedder = OpenAiCompatibleEmbedder::new(Some(secret()), config).unwrap();
    assert_no_key(&format!("{embedder:?}"));
    let docs = [
        DocumentInput::with_title("t.rs", "alpha"),
        DocumentInput::new("beta"),
    ];
    let out = embedder.embed_documents_with_usage(&docs).await.unwrap();
    assert_eq!(out.embeddings.len(), 2);
    assert_eq!(out.usage.input_tokens, 11);

    let sent = bodies(&server).await;
    assert_eq!(sent[0]["model"], "text-embedding-test");
    assert_eq!(sent[0]["dimensions"], 3);
    assert_eq!(sent[0]["encoding_format"], "float");
    assert_eq!(sent[0]["input"], json!(["t.rs\n\nalpha", "beta"]));
    let received = server.received_requests().await.unwrap();
    assert!(received[0].url.query().is_none());
}

#[tokio::test]
async fn openai_omits_dimensions_and_auth_when_not_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().unwrap();
            let n = request_count(&body);
            let data: Vec<Value> = (0..n)
                .map(|i| json!({ "embedding": [1.0, (i + 1) as f32] }))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
        })
        .mount(&server)
        .await;
    let mut config = OpenAiCompatibleConfig::new(Url::parse(&server.uri()).unwrap(), "m", 2);
    config.limits = fast_limits();
    let embedder = OpenAiCompatibleEmbedder::new(None, config).unwrap();
    let out = embedder
        .embed_documents_with_usage(&[DocumentInput::new("a")])
        .await
        .unwrap();
    assert!(out.usage.tokens_estimated);
    let received = server.received_requests().await.unwrap();
    assert!(received[0].headers.get("authorization").is_none());
    let body: Value = received[0].body_json().unwrap();
    assert!(body.get("dimensions").is_none());
}

#[tokio::test]
async fn openai_rejects_bad_indices() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [ { "index": 0, "embedding": [1.0, 0.0] }, { "index": 0, "embedding": [0.0, 1.0] } ]
        })))
        .mount(&server)
        .await;
    let mut config = OpenAiCompatibleConfig::new(Url::parse(&server.uri()).unwrap(), "m", 2);
    config.limits = fast_limits();
    let embedder = OpenAiCompatibleEmbedder::new(None, config).unwrap();
    let err = embedder
        .embed_documents(&[DocumentInput::new("a"), DocumentInput::new("b")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Response { .. }), "{err}");
}

#[tokio::test]
async fn openai_error_never_contains_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(format!("Incorrect API key provided: {}", fake_key())),
        )
        .mount(&server)
        .await;
    let mut config = OpenAiCompatibleConfig::new(Url::parse(&server.uri()).unwrap(), "m", 2);
    config.limits = fast_limits();
    let embedder = OpenAiCompatibleEmbedder::new(Some(secret()), config).unwrap();
    let err = embedder.embed_query("q").await.unwrap_err();
    assert!(matches!(err, EmbedError::Auth { .. }));
    assert_no_key(&err.to_string());
    assert_no_key(&format!("{err:?}"));
}

// ---------------------------------------------------------------- Ollama

#[tokio::test]
async fn ollama_request_shape_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().unwrap();
            let n = request_count(&body);
            let embeddings: Vec<Value> =
                (0..n).map(|i| json!([(i + 1) as f32, 1.0, 0.0])).collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "model": "nomic-test",
                "embeddings": embeddings,
                "prompt_eval_count": 9,
            }))
        })
        .mount(&server)
        .await;
    let mut config = OllamaConfig::new(Url::parse(&server.uri()).unwrap(), "nomic-test", 3);
    config.limits = fast_limits();
    let embedder = OllamaEmbedder::new(config).unwrap();
    let docs = [
        DocumentInput::new("one"),
        DocumentInput::with_title("t", "two"),
    ];
    let out = embedder.embed_documents_with_usage(&docs).await.unwrap();
    assert_eq!(out.embeddings.len(), 2);
    assert_eq!(out.usage.input_tokens, 9);
    let sent = bodies(&server).await;
    assert_eq!(sent[0]["model"], "nomic-test");
    assert_eq!(sent[0]["input"], json!(["one", "t\n\ntwo"]));
    assert_eq!(sent[0]["truncate"], false);
    assert!(sent[0].get("dimensions").is_none());
}

#[tokio::test]
async fn ollama_retries_5xx_and_checks_dimensions() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "embeddings": [[1.0, 2.0]] })),
        )
        .mount(&server)
        .await;
    let mut config = OllamaConfig::new(Url::parse(&server.uri()).unwrap(), "m", 3);
    config.limits = fast_limits();
    let embedder = OllamaEmbedder::new(config).unwrap();
    // Succeeds after one retry, but the vector has 2 dimensions, not 3.
    let err = embedder.embed_query("q").await.unwrap_err();
    assert!(matches!(err, EmbedError::Response { .. }), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
