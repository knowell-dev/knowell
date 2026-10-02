//! Test harness: a router with an in-memory token store, a memory audit
//! sink and helpers to build requests and read problem bodies.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use futures::StreamExt;
use knowell_auth::{
    Grant, Pepper, Principal, ResourceScope, Role, TokenScope, TokenScopes, UserId, issue_token,
};
use knowell_config::ServerRole;
use knowell_core::Name;
use knowell_server::{
    AppState, AppStateBuilder, BoxFuture, Engine, EngineContext, EngineError, EngineRequest,
    MemoryAuditSink, MemoryTokenStore, ServerConfig, build_router,
};
use serde_json::Value;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

pub(crate) const PORT: u16 = 7420;
pub(crate) const HOST: &str = "127.0.0.1:7420";
pub(crate) const ORIGIN: &str = "http://127.0.0.1:7420";
pub(crate) const PEPPER: &[u8] = b"fake-pepper-for-server-tests-0123";

pub(crate) fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub(crate) fn uid(k: u128) -> UserId {
    UserId::new(Uuid::from_u128(k))
}

pub(crate) fn user(k: u128) -> Principal {
    Principal::User(uid(k))
}

pub(crate) fn agent_of(k: u128) -> Principal {
    Principal::Agent {
        on_behalf_of: uid(k),
        client: n("test-agent"),
        session: knowell_auth::AgentSessionId::new(Uuid::from_u128(900 + k)),
    }
}

/// Standalone on 127.0.0.1:7420, organization `acme`, local user 1.
pub(crate) fn config() -> ServerConfig {
    let mut c = ServerConfig::new(
        ServerRole::Standalone,
        format!("127.0.0.1:{PORT}").parse().unwrap(),
        n("acme"),
    );
    c.local_user = Some(uid(1));
    c
}

/// Everything a test needs to drive the router and inspect side effects.
pub(crate) struct Harness {
    pub(crate) router: Router,
    pub(crate) state: AppState,
    pub(crate) tokens: Arc<MemoryTokenStore>,
    pub(crate) audit: Arc<MemoryAuditSink>,
}

pub(crate) fn pepper() -> Pepper {
    Pepper::new(PEPPER).unwrap()
}

/// A harness where user 1 is organization admin.
pub(crate) fn harness() -> Harness {
    harness_with(config(), |b| b)
}

pub(crate) fn harness_with(
    config: ServerConfig,
    customize: impl FnOnce(AppStateBuilder) -> AppStateBuilder,
) -> Harness {
    let tokens = Arc::new(MemoryTokenStore::new());
    tokens
        .add_grant(Grant::new(user(1), Role::Admin, ResourceScope::Organization).unwrap())
        .unwrap();
    let audit = Arc::new(MemoryAuditSink::new());
    let builder = AppState::builder(config)
        .with_token_store(tokens.clone())
        .with_pepper(pepper())
        .with_audit(audit.clone());
    let state = customize(builder).build().unwrap();
    Harness {
        router: build_router(state.clone()),
        state,
        tokens,
        audit,
    }
}

impl Harness {
    /// Issues a token for `principal` and stores it; returns the plaintext.
    pub(crate) fn token(&self, principal: Principal, scopes: &[TokenScope]) -> String {
        let now = OffsetDateTime::now_utc();
        let expires = principal.is_agent().then(|| now + time::Duration::hours(1));
        let (plain, stored) = issue_token(
            &principal,
            TokenScopes::new(scopes.iter().copied()).unwrap(),
            expires,
            now,
            &pepper(),
        )
        .unwrap();
        self.tokens.insert_token(stored).unwrap();
        plain.expose().to_owned()
    }

    pub(crate) fn admin_token(&self) -> String {
        self.token(
            user(1),
            &[TokenScope::Read, TokenScope::Write, TokenScope::Admin],
        )
    }

    pub(crate) fn grant(&self, principal: Principal, role: Role, scope: ResourceScope) {
        self.tokens
            .add_grant(Grant::new(principal, role, scope).unwrap())
            .unwrap();
    }

    pub(crate) async fn send(&self, req: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        Reply {
            status,
            headers,
            body,
        }
    }

    /// Bootstraps a local panel session: `(cookie pair, csrf token)`.
    pub(crate) async fn session(&self) -> (String, String) {
        let reply = self.send(get("/api/v1/session").build()).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let cookie = reply
            .headers
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf = reply.json()["csrfToken"].as_str().unwrap().to_owned();
        (cookie, csrf)
    }
}

/// A buffered response.
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl Reply {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not json ({}): {}", self.status, self.text()))
    }

    /// Asserts a problem+json response with `status` and `code`, including
    /// the request id in both header and body. Returns the body.
    pub(crate) fn problem(&self, status: StatusCode, code: &str) -> Value {
        assert_eq!(self.status, status, "{}", self.text());
        assert_eq!(
            self.headers["content-type"],
            "application/problem+json",
            "{}",
            self.text()
        );
        let body = self.json();
        assert_eq!(body["code"], code, "{body}");
        assert_eq!(body["status"], status.as_u16());
        assert_eq!(body["type"], format!("urn:knowell:problem:{code}"));
        assert_eq!(body["detail"], body["message"]);
        assert!(body["title"].is_string());
        let id = self.headers["x-request-id"].to_str().unwrap();
        assert_eq!(body["requestId"], id);
        body
    }
}

/// Request builder with the allowed `Host` preset.
pub(crate) struct Req {
    builder: axum::http::request::Builder,
    body: Body,
}

pub(crate) fn get(path: &str) -> Req {
    request(Method::GET, path)
}

pub(crate) fn post(path: &str) -> Req {
    request(Method::POST, path)
}

pub(crate) fn request(method: Method, path: &str) -> Req {
    Req {
        builder: Request::builder()
            .method(method)
            .uri(path)
            .header("host", HOST),
        body: Body::empty(),
    }
}

impl Req {
    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.builder = self.builder.header(name, value);
        self
    }

    pub(crate) fn origin(self) -> Self {
        self.header("origin", ORIGIN)
    }

    pub(crate) fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub(crate) fn cookie(self, cookie: &str) -> Self {
        self.header("cookie", cookie)
    }

    pub(crate) fn csrf(self, token: &str) -> Self {
        self.header("x-knowell-csrf", token)
    }

    pub(crate) fn json(mut self, value: &Value) -> Self {
        self.builder = self.builder.header("content-type", "application/json");
        self.body = Body::from(value.to_string());
        self
    }

    pub(crate) fn raw(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    pub(crate) fn build(self) -> Request<Body> {
        self.builder.body(self.body).unwrap()
    }
}

/// One recorded engine call.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub(crate) request: EngineRequest,
    pub(crate) principal: Principal,
    pub(crate) sees_everything: bool,
}

type Responder = dyn Fn(&EngineRequest) -> Result<Value, EngineError> + Send + Sync;

/// An engine that records calls and answers with a closure.
pub(crate) struct FakeEngine {
    pub(crate) calls: Mutex<Vec<Call>>,
    respond: Box<Responder>,
    delay: Duration,
}

impl FakeEngine {
    pub(crate) fn new(
        respond: impl Fn(&EngineRequest) -> Result<Value, EngineError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            respond: Box::new(respond),
            delay: Duration::ZERO,
        })
    }

    pub(crate) fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            respond: Box::new(|_| Ok(serde_json::json!({}))),
            delay,
        })
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl Engine for FakeEngine {
    fn call<'a>(
        &'a self,
        ctx: &'a EngineContext,
        request: EngineRequest,
    ) -> BoxFuture<'a, Result<Value, EngineError>> {
        Box::pin(async move {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.calls.lock().unwrap().push(Call {
                request: request.clone(),
                principal: ctx.principal.clone(),
                sees_everything: ctx.visible.is_all(),
            });
            (self.respond)(&request)
        })
    }
}

/// Reads SSE frames (`data: …\n\n`) from a streaming body.
pub(crate) struct SseReader {
    stream: axum::body::BodyDataStream,
    buffer: String,
}

impl SseReader {
    pub(crate) fn new(body: Body) -> Self {
        Self {
            stream: body.into_data_stream(),
            buffer: String::new(),
        }
    }

    /// The next event's JSON payload, or `None` when the stream ended or
    /// nothing arrived within `wait`.
    pub(crate) async fn next(&mut self, wait: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let frame: String = self.buffer.drain(..end + 2).collect();
                let data: String = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
                    .collect();
                if data.is_empty() {
                    continue;
                }
                return Some(serde_json::from_str(&data).unwrap());
            }
            let chunk = tokio::time::timeout_at(deadline, self.stream.next())
                .await
                .ok()??
                .ok()?;
            self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }
}
