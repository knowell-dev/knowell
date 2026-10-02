#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

//! Provider tests against a local mock server. No real API is ever called;
//! keys are fake values built at runtime.

use std::time::{Duration, Instant};

use knowell_judge::{
    Candidate, CohereConfig, CohereReranker, DataPolicy, HttpOptions, JudgeError, Reranker,
    TeiConfig, TeiReranker, VoyageConfig, VoyageReranker,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLOUD: DataPolicy = DataPolicy::CLOUD_ALLOWED;

fn fake_key() -> String {
    format!("KNOWELL_CANARY_{}", "z9".repeat(10))
}

fn secret() -> SecretString {
    SecretString::from(fake_key())
}

fn fast() -> HttpOptions {
    HttpOptions {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        max_retry_after: Duration::from_secs(2),
        timeout: Duration::from_secs(5),
        ..HttpOptions::default()
    }
}

fn cands() -> Vec<Candidate> {
    vec![
        Candidate::new("a", "alpha text"),
        Candidate::new("b", "beta text"),
        Candidate::new("c", "gamma text"),
    ]
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn voyage(server: &MockServer, options: HttpOptions) -> VoyageReranker {
    let mut cfg = VoyageConfig::new(secret(), "rerank-2.5").unwrap();
    cfg.base_url = url(&format!("{}/v1", server.uri()));
    cfg.options = options;
    VoyageReranker::new(cfg).unwrap()
}

fn cohere(server: &MockServer, options: HttpOptions) -> CohereReranker {
    let mut cfg = CohereConfig::new(secret(), "rerank-v3.5").unwrap();
    cfg.base_url = url(&server.uri());
    cfg.options = options;
    CohereReranker::new(cfg).unwrap()
}

fn tei(server: &MockServer, key: Option<SecretString>) -> TeiReranker {
    let mut cfg = TeiConfig::new(url(&server.uri()), "BAAI/bge-reranker-v2-m3");
    cfg.api_key = key;
    cfg.options = HttpOptions {
        max_documents: 32,
        ..fast()
    };
    TeiReranker::new(cfg).unwrap()
}

async fn body_of(server: &MockServer, n: usize) -> Value {
    let reqs = server.received_requests().await.unwrap();
    serde_json::from_slice(&reqs[n].body).unwrap()
}

async fn count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn ids(v: &[knowell_judge::Scored]) -> Vec<&str> {
    v.iter().map(|s| s.id.as_str()).collect()
}

fn voyage_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "object": "list",
        "data": [
            {"index": 0, "relevance_score": 0.2},
            {"index": 2, "relevance_score": 0.9},
            {"index": 1, "relevance_score": 0.5}
        ],
        "model": "rerank-2.5",
        "usage": {"total_tokens": 42}
    }))
}

fn cohere_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "x",
        "results": [
            {"index": 1, "relevance_score": 0.7},
            {"index": 0, "relevance_score": 0.1}
        ],
        "meta": {"billed_units": {"search_units": 1}}
    }))
}

fn tei_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!([
        {"index": 2, "score": 0.95},
        {"index": 0, "score": 0.30},
        {"index": 1, "score": 0.01}
    ]))
}

// ---------------------------------------------------------------- Voyage

#[tokio::test]
async fn voyage_request_shape_auth_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .and(header(
            "authorization",
            format!("Bearer {}", fake_key()).as_str(),
        ))
        .respond_with(voyage_ok())
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let out = r.rerank(CLOUD, "find alpha", &cands(), 2).await.unwrap();
    assert_eq!(ids(&out), ["c", "b"]);
    assert!((out[0].score - 0.9).abs() < 1e-6);

    let body = body_of(&server, 0).await;
    assert_eq!(body["model"], "rerank-2.5");
    assert_eq!(body["query"], "find alpha");
    assert_eq!(
        body["documents"],
        json!(["alpha text", "beta text", "gamma text"])
    );
    assert_eq!(body["top_k"], 2);
    assert_eq!(body["truncation"], true);
    assert_eq!(body["return_documents"], false);

    let u = r.usage();
    assert_eq!((u.requests, u.documents, u.tokens), (1, 3, 42));
    let d = r.descriptor();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("voyage", "rerank-2.5")
    );
}

#[tokio::test]
async fn voyage_truncates_documents_and_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok())
        .mount(&server)
        .await;
    let r = voyage(
        &server,
        HttpOptions {
            max_document_chars: 4,
            max_query_chars: 3,
            ..fast()
        },
    );
    let docs = vec![
        Candidate::new("a", "héllo wörld"),
        Candidate::new("b", "ab"),
        Candidate::new("c", "日本語テキスト"),
    ];
    r.rerank(CLOUD, "queryxyz", &docs, 3).await.unwrap();
    let body = body_of(&server, 0).await;
    assert_eq!(body["documents"], json!(["héll", "ab", "日本語テ"]));
    assert_eq!(body["query"], "que");
}

#[tokio::test]
async fn voyage_refuses_under_local_only_without_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok())
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let err = r
        .rerank(DataPolicy::LOCAL_ONLY, "q", &cands(), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, JudgeError::PolicyRefused { ref provider } if provider == "voyage"));
    assert!(err.to_string().contains("local-only"));
    assert_eq!(count(&server).await, 0);
}

#[tokio::test]
async fn voyage_retries_503_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok())
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let out = r.rerank(CLOUD, "q", &cands(), 3).await.unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(count(&server).await, 3);
    assert_eq!(r.usage().requests, 3);
}

#[tokio::test]
async fn gives_up_after_max_retries_with_last_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"detail": "boom"})))
        .mount(&server)
        .await;
    let r = voyage(
        &server,
        HttpOptions {
            max_retries: 2,
            ..fast()
        },
    );
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(err, JudgeError::Http { status: 500, ref message } if message == "boom"));
    assert_eq!(count(&server).await, 3);
}

#[tokio::test]
async fn honours_retry_after_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok())
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let start = Instant::now();
    r.rerank(CLOUD, "q", &cands(), 1).await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(950),
        "waited {:?}",
        start.elapsed()
    );
    assert_eq!(count(&server).await, 2);
}

#[tokio::test]
async fn retry_after_beyond_cap_fails_fast() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let start = Instant::now();
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(
        err,
        JudgeError::RateLimited {
            retry_after_secs: Some(3600)
        }
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(count(&server).await, 1);
}

#[tokio::test]
async fn client_errors_are_not_retried_and_map() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"detail": "bad credentials"})),
        )
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(err, JudgeError::Unauthorized { status: 401, .. }));
    assert_eq!(count(&server).await, 1);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"detail": "too long"})))
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(err, JudgeError::Http { status: 400, ref message } if message == "too long"));
}

#[tokio::test]
async fn error_body_echoing_the_key_is_masked() {
    let server = MockServer::start().await;
    let key = fake_key();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"detail": format!("invalid key {key} for request")})),
        )
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    let shown = format!("{err} | {err:?}");
    assert!(!shown.contains(&key), "key leaked: {shown}");
    assert!(
        !shown.contains("KNOWELL_CANARY"),
        "key fragment leaked: {shown}"
    );
    assert!(shown.contains("[REDACTED]"));

    // Same for plain-text bodies and for a 5xx that ends the retries.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string(format!("gateway saw {key}")))
        .mount(&server)
        .await;
    let r = voyage(&server, fast());
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(!format!("{err} {err:?}").contains("KNOWELL_CANARY"));
}

#[tokio::test]
async fn debug_output_never_contains_the_key() {
    let server = MockServer::start().await;
    let v = voyage(&server, fast());
    let c = cohere(&server, fast());
    let mut vc = VoyageConfig::new(secret(), "m").unwrap();
    vc.base_url = url(&server.uri());
    let mut cc = CohereConfig::new(secret(), "m").unwrap();
    cc.base_url = url(&server.uri());
    let mut tc = TeiConfig::new(url(&server.uri()), "m");
    tc.api_key = Some(secret());
    let t = TeiReranker::new(tc.clone()).unwrap();
    for text in [
        format!("{v:?}"),
        format!("{c:?}"),
        format!("{t:?}"),
        format!("{vc:?}"),
        format!("{cc:?}"),
        format!("{tc:?}"),
    ] {
        assert!(!text.contains("KNOWELL_CANARY"), "{text}");
    }
}

#[tokio::test]
async fn timeout_maps_to_timeout_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok().set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let r = voyage(
        &server,
        HttpOptions {
            timeout: Duration::from_millis(50),
            ..fast()
        },
    );
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(err, JudgeError::Timeout), "{err:?}");
}

#[tokio::test]
async fn connection_failure_is_a_transport_error_without_key() {
    // Port 1 on loopback: nothing listens.
    let mut cfg = VoyageConfig::new(secret(), "m").unwrap();
    cfg.base_url = url("http://127.0.0.1:1");
    cfg.options = HttpOptions {
        max_retries: 0,
        ..fast()
    };
    let r = VoyageReranker::new(cfg).unwrap();
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(
        matches!(err, JudgeError::Transport(_) | JudgeError::Timeout),
        "{err:?}"
    );
    assert!(!format!("{err:?}").contains("KNOWELL_CANARY"));
}

#[tokio::test]
async fn too_many_candidates_is_an_error_and_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok())
        .mount(&server)
        .await;
    let r = voyage(
        &server,
        HttpOptions {
            max_documents: 2,
            ..fast()
        },
    );
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(
        err,
        JudgeError::TooManyCandidates { given: 3, max: 2 }
    ));
    assert_eq!(count(&server).await, 0);
}

#[tokio::test]
async fn invalid_responses_are_rejected() {
    for body in [
        json!({"data": [{"index": 9, "relevance_score": 0.5}]}),
        json!({"data": [{"index": 0, "relevance_score": 0.5}, {"index": 0, "relevance_score": 0.4}]}),
        json!({"data": []}),
        json!({"unexpected": true}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let r = voyage(&server, fast());
        let err = r.rerank(CLOUD, "q", &cands(), 3).await.unwrap_err();
        assert!(matches!(err, JudgeError::InvalidResponse(_)), "{err:?}");
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json {"))
        .mount(&server)
        .await;
    let err = voyage(&server, fast())
        .rerank(CLOUD, "q", &cands(), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, JudgeError::InvalidResponse(_)));
}

#[tokio::test]
async fn concurrency_limit_serialises_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(voyage_ok().set_delay(Duration::from_millis(150)))
        .mount(&server)
        .await;
    let r = std::sync::Arc::new(voyage(
        &server,
        HttpOptions {
            max_concurrency: 1,
            ..fast()
        },
    ));
    let start = Instant::now();
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let r = std::sync::Arc::clone(&r);
            tokio::spawn(async move { r.rerank(CLOUD, "q", &cands(), 1).await })
        })
        .collect();
    for h in handles {
        assert!(h.await.unwrap().is_ok());
    }
    assert!(
        start.elapsed() >= Duration::from_millis(440),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn cloud_providers_reject_plain_http_to_remote_hosts_and_leaky_urls() {
    let mut cfg = VoyageConfig::new(secret(), "m").unwrap();
    cfg.base_url = url("http://example.com/v1");
    assert!(matches!(
        VoyageReranker::new(cfg),
        Err(JudgeError::InvalidConfig(_))
    ));

    let mut cfg = CohereConfig::new(secret(), "m").unwrap();
    cfg.base_url = url("http://example.com");
    assert!(matches!(
        CohereReranker::new(cfg),
        Err(JudgeError::InvalidConfig(_))
    ));

    let mut cfg = VoyageConfig::new(secret(), "m").unwrap();
    cfg.base_url = url("https://user:pw@example.com/v1");
    assert!(matches!(
        VoyageReranker::new(cfg),
        Err(JudgeError::InvalidConfig(_))
    ));

    let mut cfg = VoyageConfig::new(secret(), "m").unwrap();
    cfg.base_url = url("https://example.com/v1?api_key=abc");
    assert!(matches!(
        VoyageReranker::new(cfg),
        Err(JudgeError::InvalidConfig(_))
    ));

    let mut cfg = VoyageConfig::new(secret(), "m").unwrap();
    cfg.options.max_concurrency = 0;
    assert!(matches!(
        VoyageReranker::new(cfg),
        Err(JudgeError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn empty_candidates_make_no_call() {
    let server = MockServer::start().await;
    let r = voyage(&server, fast());
    assert!(r.rerank(CLOUD, "q", &[], 3).await.unwrap().is_empty());
    assert_eq!(count(&server).await, 0);
}

// ---------------------------------------------------------------- Cohere

#[tokio::test]
async fn cohere_request_shape_auth_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/rerank"))
        .and(header(
            "authorization",
            format!("Bearer {}", fake_key()).as_str(),
        ))
        .respond_with(cohere_ok())
        .mount(&server)
        .await;
    let mut cfg = CohereConfig::new(secret(), "rerank-v3.5").unwrap();
    cfg.base_url = url(&server.uri());
    cfg.options = fast();
    cfg.max_tokens_per_doc = Some(512);
    let r = CohereReranker::new(cfg).unwrap();
    let out = r.rerank(CLOUD, "find beta", &cands(), 2).await.unwrap();
    assert_eq!(ids(&out), ["b", "a"]);

    let body = body_of(&server, 0).await;
    assert_eq!(body["model"], "rerank-v3.5");
    assert_eq!(body["query"], "find beta");
    assert_eq!(
        body["documents"],
        json!(["alpha text", "beta text", "gamma text"])
    );
    assert_eq!(body["top_n"], 2);
    assert_eq!(body["max_tokens_per_doc"], 512);
    assert_eq!(r.usage().search_units, 1);
    assert_eq!(r.descriptor().provider, "cohere");
}

#[tokio::test]
async fn cohere_omits_max_tokens_by_default_and_truncates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(cohere_ok())
        .mount(&server)
        .await;
    let r = cohere(
        &server,
        HttpOptions {
            max_document_chars: 5,
            ..fast()
        },
    );
    r.rerank(CLOUD, "q", &cands(), 3).await.unwrap();
    let body = body_of(&server, 0).await;
    assert!(body.get("max_tokens_per_doc").is_none());
    assert_eq!(body["documents"], json!(["alpha", "beta ", "gamma"]));
}

#[tokio::test]
async fn cohere_policy_retry_and_error_mapping() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(cohere_ok())
        .mount(&server)
        .await;
    let r = cohere(&server, fast());
    let err = r
        .rerank(DataPolicy::LOCAL_ONLY, "q", &cands(), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, JudgeError::PolicyRefused { ref provider } if provider == "cohere"));
    assert_eq!(count(&server).await, 0);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(cohere_ok())
        .mount(&server)
        .await;
    let r = cohere(&server, fast());
    assert_eq!(r.rerank(CLOUD, "q", &cands(), 2).await.unwrap().len(), 2);
    assert_eq!(count(&server).await, 3);

    let server = MockServer::start().await;
    let key = fake_key();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message": format!("token {key} denied")})),
        )
        .mount(&server)
        .await;
    let r = cohere(&server, fast());
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(matches!(err, JudgeError::Unauthorized { status: 403, .. }));
    assert!(!format!("{err} {err:?}").contains("KNOWELL_CANARY"));
}

// ------------------------------------------------------------------- TEI

#[tokio::test]
async fn tei_request_shape_without_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(tei_ok())
        .mount(&server)
        .await;
    let r = tei(&server, None);
    let out = r.rerank(CLOUD, "find gamma", &cands(), 2).await.unwrap();
    assert_eq!(ids(&out), ["c", "a"]);

    let body = body_of(&server, 0).await;
    assert_eq!(body["query"], "find gamma");
    assert_eq!(
        body["texts"],
        json!(["alpha text", "beta text", "gamma text"])
    );
    assert_eq!(body["raw_scores"], false);
    assert_eq!(body["return_text"], false);
    assert_eq!(body["truncate"], true);
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs[0].headers.get("authorization").is_none());
    assert_eq!(r.descriptor().provider, "tei");
}

#[tokio::test]
async fn tei_sends_bearer_when_configured_and_truncates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header(
            "authorization",
            format!("Bearer {}", fake_key()).as_str(),
        ))
        .respond_with(tei_ok())
        .mount(&server)
        .await;
    let mut cfg = TeiConfig::new(url(&server.uri()), "m");
    cfg.api_key = Some(secret());
    cfg.options = HttpOptions {
        max_document_chars: 3,
        ..fast()
    };
    let r = TeiReranker::new(cfg).unwrap();
    r.rerank(CLOUD, "q", &cands(), 3).await.unwrap();
    assert_eq!(
        body_of(&server, 0).await["texts"],
        json!(["alp", "bet", "gam"])
    );
}

#[tokio::test]
async fn tei_local_endpoint_is_allowed_under_local_only() {
    let server = MockServer::start().await; // 127.0.0.1
    Mock::given(method("POST"))
        .respond_with(tei_ok())
        .mount(&server)
        .await;
    let r = tei(&server, None);
    assert!(
        r.rerank(DataPolicy::LOCAL_ONLY, "q", &cands(), 1)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn tei_remote_or_overridden_endpoint_is_refused_under_local_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(tei_ok())
        .mount(&server)
        .await;
    let mut cfg = TeiConfig::new(url(&server.uri()), "m");
    cfg.assume_local = Some(false);
    let r = TeiReranker::new(cfg).unwrap();
    let err = r
        .rerank(DataPolicy::LOCAL_ONLY, "q", &cands(), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, JudgeError::PolicyRefused { .. }));
    assert_eq!(count(&server).await, 0);

    // A non-loopback host is "not local" by default; nothing is contacted.
    let cfg = TeiConfig::new(url("http://tei.internal.example:8080"), "m");
    let r = TeiReranker::new(cfg).unwrap();
    let err = r
        .rerank(DataPolicy::LOCAL_ONLY, "q", &cands(), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, JudgeError::PolicyRefused { .. }));
}

#[tokio::test]
async fn tei_retries_and_maps_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(json!({"error": "model is overloaded", "error_type": "overloaded"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(tei_ok())
        .mount(&server)
        .await;
    let r = tei(&server, None);
    assert_eq!(r.rerank(CLOUD, "q", &cands(), 3).await.unwrap().len(), 3);
    assert_eq!(count(&server).await, 2);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(413).set_body_json(
                json!({"error": "batch size too large", "error_type": "validation"}),
            ),
        )
        .mount(&server)
        .await;
    let r = tei(&server, None);
    let err = r.rerank(CLOUD, "q", &cands(), 1).await.unwrap_err();
    assert!(
        matches!(err, JudgeError::Http { status: 413, ref message } if message == "batch size too large")
    );
}

#[tokio::test]
async fn tei_default_max_documents_is_32() {
    let server = MockServer::start().await;
    let cfg = TeiConfig::new(url(&server.uri()), "m");
    assert_eq!(cfg.options.max_documents, 32);
    let r = TeiReranker::new(cfg).unwrap();
    let many: Vec<_> = (0..33)
        .map(|i| Candidate::new(format!("{i}"), "t"))
        .collect();
    let err = r.rerank(CLOUD, "q", &many, 1).await.unwrap_err();
    assert!(matches!(
        err,
        JudgeError::TooManyCandidates { given: 33, max: 32 }
    ));
}
